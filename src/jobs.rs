//! In-memory async-`consult` job registry — backs `consult_submit` and the shared
//! `get` / `cancel` / `list` verbs (which also serve batch handles).
//!
//! The async counterpart to the synchronous `consult`: `consult_submit` returns a job
//! id immediately, the consultation runs as a spawned task, and `get` collects the
//! answer when it lands. This is the *same* diskless, persistence-shaped state as
//! [`crate::session::SessionStore`] — an `Arc<Mutex<LruCache>>`, capacity-LRU, no TTL,
//! the mutex never held across an `.await` — only the value is a job's live status
//! instead of a conversation thread. kaibo writes nothing to disk: a job lives for the
//! session and dies with the process, exactly as the spawned sub-agent it replaces did.
//!
//! The store is deliberately ignorant of *what* a job is. It spawns any
//! `Future<Output = Result<JobResult, String>>` and tracks its terminal state, so the
//! consult-specific rendering (provenance footer, failure classification) stays in
//! `server.rs` and this module stays offline-testable with trivial futures.
//!
//! **Eviction is capacity-LRU, like sessions** — a job is dropped only when a newer
//! submit pushes it past the cap as the least-recently-used; touching it (`get`/
//! `cancel`) promotes it. A still-*running* job that gets evicted is **aborted** (its
//! `Job` drop aborts the task): once the store drops a job its id is unreachable
//! (`get`/`cancel` return not-found), so letting an orphaned consult run on would just
//! burn provider tokens against a cell nobody can read. The spawned task owns its own
//! `Arc` of the status cell, so the cell stays valid until the task actually stops —
//! no panic, no disk.

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::task::AbortHandle;

/// A completed consultation, ready to render back to the calling agent. `answer`
/// already carries its provenance footer; `report` is the explorer's aggregated
/// report when the submit asked for it (`include_report`), else `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobResult {
    pub answer: String,
    pub report: Option<String>,
}

/// Where a job is in its lifecycle, as one `get` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// The spawned task is still working.
    Running,
    /// The consultation finished and produced an answer.
    Done(JobResult),
    /// The consultation failed; the string is the already-rendered failure text
    /// (detail + guidance), so the tool layer wraps it without re-classifying.
    Failed(String),
    /// `cancel` aborted the task before it finished.
    Canceled,
}

/// A point-in-time view of a job for rendering: its state plus the metadata a poll
/// line wants (which cast/models, how long it's been running).
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub state: JobState,
    pub label: String,
    pub age: Duration,
}

/// What `cancel` did, so the tool layer can word the reply honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job was running and is now aborted + marked canceled.
    Canceled,
    /// The job had already reached a terminal state — nothing to abort.
    AlreadyFinished,
    /// No such job id (never existed, or evicted).
    Unknown,
}

/// One job: a shared status cell the spawned task writes, plus the handle to abort it
/// and the metadata for rendering. The `state` is its own `Arc<Mutex<_>>` rather than
/// living under the store's map lock, so the task can finish (and set its result)
/// without contending for the whole registry — and survives the entry's eviction.
struct Job {
    state: Arc<Mutex<JobState>>,
    abort: AbortHandle,
    label: String,
    started: Instant,
}

impl Drop for Job {
    /// Abort the task when its `Job` is dropped — i.e. when the store evicts it (LRU) or
    /// the whole store goes away. Dropping an `AbortHandle` does *not* abort the task on
    /// its own, so without this an evicted-but-still-running consult would run to
    /// completion against an unreachable cell, burning provider tokens for an answer no
    /// `get` can ever return. A finished or already-canceled job's abort is a harmless
    /// no-op. `get`/`cancel`/`list` only clone the `Arc<Job>` transiently under the store
    /// lock, so this fires on the *last* reference — true unreachability — not on a poll.
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// A drop guard for the spawned task: if the task's future ends *without* landing a
/// result — a panic (we build `panic = unwind`, so tokio catches it and the state would
/// otherwise sit at `Running` forever) or an abort that drops the future at its await
/// point — record a terminal failure instead of leaving `get` to report "still running"
/// with no end. The task disarms it on the normal path so it never overwrites a real
/// outcome. On an *abort* the cell is usually unreachable anyway (canceled, or evicted),
/// so the write is harmless; on a *panic* of a still-live job it's the difference between
/// an eternal hang and an honest failure.
struct FinishGuard {
    state: Arc<Mutex<JobState>>,
    armed: bool,
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Reached only on an unwind/abort before the task disarmed us. Lock the state
        // (the task holds no lock at its await point, so no deadlock) and fail it iff it
        // never reached a terminal state.
        if let Ok(mut s) = self.state.lock() {
            if matches!(*s, JobState::Running) {
                *s = JobState::Failed(
                    "the consultation task ended without completing (it panicked or was \
                     aborted)"
                        .to_string(),
                );
            }
        }
    }
}

/// A cap-based LRU of async consultations, keyed by a kaibo-minted job id (`job-N`).
#[derive(Clone)]
pub struct JobStore {
    inner: Arc<Mutex<LruCache<String, Arc<Job>>>>,
    next_id: Arc<AtomicU64>,
}

impl JobStore {
    /// A store holding at most `capacity` jobs (running + finished-but-uncollected).
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Spawn `fut` as a job and return its id immediately. `label` is the human-facing
    /// cast/model summary a poll line shows. The future's `Ok`/`Err` becomes the job's
    /// terminal `Done`/`Failed` state; an abort (via [`cancel`](Self::cancel)) leaves it
    /// `Canceled` because the task never runs its tail.
    pub fn submit<F>(&self, label: impl Into<String>, fut: F) -> String
    where
        F: Future<Output = Result<JobResult, String>> + Send + 'static,
    {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("job-{n}");

        let state = Arc::new(Mutex::new(JobState::Running));
        let task_state = state.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            // Armed until the future returns: if it panics or is aborted at its await
            // point, this guard records a terminal failure instead of leaving the job
            // stuck `Running`. Disarmed once `fut.await` returns normally below.
            let mut guard = FinishGuard {
                state: task_state.clone(),
                armed: true,
            };
            let outcome = fut.await;
            {
                let mut s = task_state.lock().expect("job state mutex poisoned");
                // Only land the result if a `cancel` didn't race in while we were
                // finishing — a caller who asked to cancel gets `Canceled`, not a
                // surprise late answer.
                if matches!(*s, JobState::Running) {
                    let succeeded = outcome.is_ok();
                    *s = match outcome {
                        Ok(result) => JobState::Done(result),
                        Err(text) => JobState::Failed(text),
                    };
                    drop(s); // release the state lock before the log emit

                    // Completion signal at **Warn** — kaibo's "the calling model should
                    // see this" level (not severity): a finished/failed job is exactly
                    // what a `wait` drains by default, and the `mcp_log` bridge also
                    // mirrors it to a watching client. Still advisory — no MCP primitive
                    // wakes the agent, so polling/`wait` stays the contract. (A canceled
                    // job never reaches here — its task is aborted — no ping for it.)
                    if succeeded {
                        tracing::warn!(target: "kaibo::jobs", job = %task_id, "async job finished — collect it with `get`");
                    } else {
                        tracing::warn!(target: "kaibo::jobs", job = %task_id, "async job failed — `get` it for the reason");
                    }
                }
            }
            // The future returned (no panic) — disarm so the guard's Drop is a no-op and
            // can't overwrite the outcome (or a raced `Canceled`).
            guard.armed = false;
        });

        let job = Arc::new(Job {
            state,
            abort: handle.abort_handle(),
            label: label.into(),
            started: Instant::now(),
        });
        self.lock().put(id.clone(), job);
        id
    }

    /// Snapshot a job's state for rendering — `None` if the id is unknown (never
    /// existed, or evicted). Touching the job promotes it to most-recently-used.
    pub fn get(&self, id: &str) -> Option<JobSnapshot> {
        let job = self.lock().get(id).cloned()?;
        let state = job.state.lock().expect("job state mutex poisoned").clone();
        Some(JobSnapshot {
            state,
            label: job.label.clone(),
            age: job.started.elapsed(),
        })
    }

    /// Abort a running job and mark it `Canceled`. Idempotent on an already-canceled
    /// job; reports `AlreadyFinished` for one that already produced a result or failure.
    /// Promotes the job (a cancel means you still hold the handle).
    pub fn cancel(&self, id: &str) -> CancelOutcome {
        let Some(job) = self.lock().get(id).cloned() else {
            return CancelOutcome::Unknown;
        };
        let mut state = job.state.lock().expect("job state mutex poisoned");
        match *state {
            JobState::Running => {
                job.abort.abort();
                *state = JobState::Canceled;
                CancelOutcome::Canceled
            }
            JobState::Canceled => CancelOutcome::Canceled,
            JobState::Done(_) | JobState::Failed(_) => CancelOutcome::AlreadyFinished,
        }
    }

    /// Snapshot every live job, most-recently-touched first — the order the unified
    /// `list` shows them. A read: it promotes nothing and takes no job's state lock for
    /// longer than the clone.
    pub fn list(&self) -> Vec<(String, JobSnapshot)> {
        self.lock()
            .iter()
            .map(|(id, job)| {
                let state = job.state.lock().expect("job state mutex poisoned").clone();
                (
                    id.clone(),
                    JobSnapshot {
                        state,
                        label: job.label.clone(),
                        age: job.started.elapsed(),
                    },
                )
            })
            .collect()
    }

    /// Number of live jobs in the registry. For tests and diagnostics.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when no jobs are tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The only work done under this lock is `LruCache` ops on owned values, which
    /// don't panic, so poisoning is effectively unreachable — treat it as the
    /// build-time bug it would be rather than masking it. Mirrors [`SessionStore`].
    fn lock(&self) -> std::sync::MutexGuard<'_, LruCache<String, Arc<Job>>> {
        self.inner.lock().expect("job store mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn done(answer: &str) -> JobResult {
        JobResult {
            answer: answer.to_string(),
            report: None,
        }
    }

    /// Poll `get` until the job leaves `Running`, or panic after a generous bound — the
    /// spawned task needs a runtime tick to land its result, so tests can't read it
    /// synchronously right after `submit`. Returns the terminal state.
    async fn await_terminal(store: &JobStore, id: &str) -> JobState {
        for _ in 0..1000 {
            match store.get(id).map(|s| s.state) {
                Some(JobState::Running) | None => tokio::task::yield_now().await,
                Some(terminal) => return terminal,
            }
        }
        panic!("job {id} never left Running");
    }

    #[tokio::test]
    async fn unknown_job_has_no_snapshot() {
        let store = JobStore::new(cap(4));
        assert!(store.get("job-404").is_none());
        assert_eq!(store.cancel("job-404"), CancelOutcome::Unknown);
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn a_ready_future_lands_as_done() {
        let store = JobStore::new(cap(4));
        let id = store.submit("cast `x`", async { Ok(done("the answer")) });
        assert_eq!(
            await_terminal(&store, &id).await,
            JobState::Done(done("the answer"))
        );
    }

    #[tokio::test]
    async fn a_failing_future_lands_as_failed() {
        let store = JobStore::new(cap(4));
        let id = store.submit("cast `x`", async { Err("provider exploded".to_string()) });
        assert_eq!(
            await_terminal(&store, &id).await,
            JobState::Failed("provider exploded".to_string())
        );
    }

    #[tokio::test]
    async fn an_in_flight_job_reads_as_running_then_done() {
        let store = JobStore::new(cap(4));
        // Gate the future on a channel the test holds, so "Running" is observable
        // deterministically rather than by racing the scheduler.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = store.submit("cast `x`", async move {
            let _ = rx.await;
            Ok(done("eventually"))
        });
        assert_eq!(
            store.get(&id).map(|s| s.state),
            Some(JobState::Running),
            "a gated job must read as Running before it's released"
        );
        tx.send(()).unwrap();
        assert_eq!(
            await_terminal(&store, &id).await,
            JobState::Done(done("eventually"))
        );
    }

    #[tokio::test]
    async fn submitting_past_capacity_evicts_the_least_recently_used() {
        let store = JobStore::new(cap(2));
        let a = store.submit("a", async { Ok(done("a")) });
        let b = store.submit("b", async { Ok(done("b")) });
        // Touch `a` so `b` becomes the least-recently-used.
        let _ = store.get(&a);
        let c = store.submit("c", async { Ok(done("c")) });

        assert!(
            store.get(&b).is_none(),
            "b was the LRU and should be evicted"
        );
        assert!(
            store.get(&a).is_some(),
            "a was just touched and must survive"
        );
        assert!(store.get(&c).is_some(), "c is newest");
        assert_eq!(store.len(), 2);
    }

    #[tokio::test]
    async fn cancel_aborts_a_running_job_and_marks_it_canceled() {
        let store = JobStore::new(cap(4));
        // A future that would run forever if not aborted — its tail must never land.
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = store.submit("cast `x`", async move {
            let _ = rx.await;
            Ok(done("should never be seen"))
        });
        assert_eq!(store.cancel(&id), CancelOutcome::Canceled);
        assert_eq!(store.get(&id).map(|s| s.state), Some(JobState::Canceled));
        // Give the runtime ticks; an aborted task must not overwrite Canceled.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store.get(&id).map(|s| s.state),
            Some(JobState::Canceled),
            "an aborted task must never clobber the Canceled state with a late result"
        );
    }

    #[tokio::test]
    async fn cancel_on_a_finished_job_reports_already_finished() {
        let store = JobStore::new(cap(4));
        let id = store.submit("cast `x`", async { Ok(done("fast")) });
        let _ = await_terminal(&store, &id).await;
        assert_eq!(store.cancel(&id), CancelOutcome::AlreadyFinished);
    }

    #[tokio::test]
    async fn eviction_aborts_a_still_running_job() {
        use std::sync::atomic::{AtomicBool, Ordering as AOrd};
        let store = JobStore::new(cap(1));
        // job-1's tail sets this flag *if* it ever runs to completion. It shouldn't —
        // eviction must abort it before the gate is released.
        let ran_tail = Arc::new(AtomicBool::new(false));
        let flag = ran_tail.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _evicted = store.submit("a", async move {
            let _ = rx.await;
            flag.store(true, AOrd::SeqCst);
            Ok(done("a"))
        });
        // A second submit at cap 1 evicts the first → its `Job` drops → task aborts.
        let _b = store.submit("b", async { Ok(done("b")) });
        // Release the gate; an aborted task must never run its tail.
        let _ = tx.send(());
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        assert!(
            !ran_tail.load(AOrd::SeqCst),
            "an evicted running job must be aborted, not run to completion (token waste)"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_lands_as_failed_not_stuck_running() {
        // tokio catches the spawned task's panic (we don't await its JoinHandle); without
        // the FinishGuard the state would sit at Running forever. The guard turns the
        // unwind into a terminal Failed, so `await_terminal` resolves instead of spinning.
        let store = JobStore::new(cap(4));
        let id = store.submit("cast `x`", async { panic!("boom in the consult loop") });
        assert!(
            matches!(await_terminal(&store, &id).await, JobState::Failed(_)),
            "a panicking task must land as Failed, not hang in Running forever"
        );
    }

    #[tokio::test]
    async fn list_returns_every_live_job_newest_first() {
        let store = JobStore::new(cap(4));
        let a = store.submit("cast `a`", async { Ok(done("a")) });
        let b = store.submit("cast `b`", async { Ok(done("b")) });
        let listed: Vec<String> = store.list().into_iter().map(|(id, _)| id).collect();
        // Most-recently-submitted first: b before a.
        assert_eq!(listed, vec![b, a]);
    }

    #[tokio::test]
    async fn ids_are_unique_and_monotonic() {
        let store = JobStore::new(cap(8));
        let a = store.submit("a", async { Ok(done("a")) });
        let b = store.submit("b", async { Ok(done("b")) });
        assert_ne!(a, b);
        assert_eq!(a, "job-1");
        assert_eq!(b, "job-2");
    }
}
