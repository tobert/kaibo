//! In-memory async-`consult` job registry — backs `consult_submit` and the shared
//! `job_get` / `job_cancel` / `job_list` verbs (which also serve batch handles).
//!
//! The async counterpart to the synchronous `consult`: `consult_submit` returns a job
//! id immediately, the consultation runs as a spawned task, and `job_get` collects the
//! answer when it lands. This is the *same* diskless, persistence-shaped state as
//! [`crate::session::SessionStore`] — an `Arc<Mutex<LruCache>>`, capacity-LRU, no TTL,
//! the mutex never held across an `.await` — only the value is a job's live status
//! instead of a conversation thread. The tasks live in this process. With persistence on,
//! a [`Ledger`] also records each job in the state db (`store/jobs.rs`): the answer
//! survives a restart, another kaibo process can read it, and a job whose owner died reads
//! as interrupted. Memory answers first; the ledger answers for a job this process does
//! not hold.
//!
//! The store is deliberately ignorant of *what* a job is. It spawns any
//! `Future<Output = Result<JobResult, String>>` and tracks its terminal state, so the
//! consult-specific rendering (provenance footer, failure classification) stays in
//! `server.rs` and this module stays offline-testable with trivial futures.
//!
//! **Eviction is capacity-LRU, like sessions** — a job is dropped only when a newer
//! submit pushes it past the cap as the least-recently-used; touching it (`job_get`/
//! `job_cancel`) promotes it. A still-*running* job that gets evicted is **aborted** (its
//! `Job` drop aborts the task): once the store drops a job its id is unreachable
//! (`job_get`/`job_cancel` return not-found), so letting an orphaned consult run on would just
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

use crate::progress::ProgressLog;
use crate::store::jobs::{JobRow, JobStatus, HEARTBEAT_EVERY_SECS};
use crate::store::SessionStore;

/// A completed job, ready to render back to the calling agent. `answer` already
/// carries its provenance footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobResult {
    pub answer: String,
}

/// Where a job is in its lifecycle, as one `job_get` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// The spawned task is still working.
    Running,
    /// The consultation finished and produced an answer.
    Done(JobResult),
    /// The consultation failed; the string is the already-rendered failure text
    /// (detail + guidance), so the tool layer wraps it without re-classifying.
    Failed(String),
    /// `job_cancel` aborted the task before it finished.
    Canceled,
}

/// A point-in-time view of a job for rendering: its state plus the metadata a poll
/// line wants (which cast/models, how long it's been running).
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub state: JobState,
    pub label: String,
    pub age: Duration,
    /// The most recent progress beat — `(one-liner, beat count)` — for a still-running
    /// job, or `None` if it hasn't emitted yet (or already finished). `job_get`/`job_list`
    /// echo it so a poller sees forward motion without reaching for `job_wait`. See
    /// [`crate::progress::ProgressLog`].
    pub last_progress: Option<(String, u64)>,
    /// How fast each role's model is answering on a still-running job — the
    /// [`ProgressLog::chat_summary`] clause — or `None` before the first call or once
    /// the job is terminal. Rendered beside `last_progress`, so a poller can tell a slow
    /// backend from a busy one.
    pub chat: Option<String>,
}

/// What `job_cancel` did, so the tool layer can word the reply honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job was running and is now aborted + marked canceled.
    Canceled,
    /// The job had already reached a terminal state — nothing to abort.
    AlreadyFinished,
    /// Another process runs the job. The cancel is recorded, and that process stops the
    /// job at its next heartbeat (within about ten seconds).
    Requested,
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
    /// The job's progress recorder — the same [`ProgressLog`] the spawned consult emits
    /// through, so `job_get`/`job_list` can echo the latest beat. Shared (`Arc`) with the running
    /// task's sink; reading `latest()` here never blocks the task (a tiny `Mutex`).
    progress: Arc<ProgressLog>,
}

impl Drop for Job {
    /// Abort the task when its `Job` is dropped — i.e. when the store evicts it (LRU) or
    /// the whole store goes away. Dropping an `AbortHandle` does *not* abort the task on
    /// its own, so without this an evicted-but-still-running consult would run to
    /// completion against an unreachable cell, burning provider tokens for an answer no
    /// `job_get` can ever return. A finished or already-canceled job's abort is a harmless
    /// no-op. `job_get`/`job_cancel`/`job_list` only clone the `Arc<Job>` transiently under the store
    /// lock, so this fires on the *last* reference — true unreachability — not on a poll.
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// A drop guard for the spawned task: if the task's future ends *without* landing a
/// result — a panic (we build `panic = unwind`, so tokio catches it and the state would
/// otherwise sit at `Running` forever) or an abort that drops the future at its await
/// point — record a terminal failure instead of leaving `job_get` to report "still running"
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
    ledger: Option<Ledger>,
    heartbeat_started: Arc<std::sync::atomic::AtomicBool>,
}

/// The state-db side of a [`JobStore`]: the store handle and this process's owner token,
/// which stamps every row this process creates and heartbeats.
#[derive(Clone)]
struct Ledger {
    store: SessionStore,
    owner: Arc<str>,
}

/// A token no other process shares: its pid plus the start time in nanoseconds.
fn owner_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos:x}-{n}", std::process::id())
}

/// `job-12` to `12`; `None` for anything else.
fn job_number(id: &str) -> Option<i64> {
    id.strip_prefix("job-")?.parse().ok()
}

impl JobStore {
    /// A store holding at most `capacity` jobs (running + finished-but-uncollected).
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
            next_id: Arc::new(AtomicU64::new(1)),
            ledger: None,
            heartbeat_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Record jobs in `store` as well as in memory. Call once, before the first submit.
    pub fn with_ledger(mut self, store: SessionStore) -> Self {
        self.ledger = Some(Ledger {
            store,
            owner: owner_token().into(),
        });
        self
    }

    /// Whether finished jobs outlive this process.
    pub fn is_durable(&self) -> bool {
        self.ledger.is_some()
    }

    /// Mint the id for a new job and, with a ledger, record it as running. A ledger that
    /// cannot be written is logged and the job gets a process-local id: the consultation
    /// still runs, it just will not be visible to other processes.
    pub async fn mint(&self, kind: &str, label: &str) -> String {
        if let Some(ledger) = &self.ledger {
            match ledger.store.job_create(kind, label, &ledger.owner).await {
                Ok(n) => return format!("job-{n}"),
                Err(e) => tracing::warn!(
                    target: "kaibo::jobs",
                    error = %e,
                    "job ledger write failed; this job is visible to this process only"
                ),
            }
        }
        self.local_id()
    }

    /// An id only this process knows. With a ledger it lives in its own namespace
    /// (`job-local-N`), so it can never address another process's row.
    fn local_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        if self.ledger.is_some() {
            format!("job-local-{n}")
        } else {
            format!("job-{n}")
        }
    }

    /// Spawn `fut` as a job and return its id immediately. `label` is the human-facing
    /// cast/model summary a poll line shows; `progress` is the recorder the running phase
    /// emits through, kept so `job_get`/`job_list` can echo the latest beat (pass it the *same*
    /// `Arc` the consult's `ConsultConfig.progress` holds). The future's `Ok`/`Err`
    /// becomes the job's terminal `Done`/`Failed` state; an abort (via
    /// [`cancel`](Self::cancel)) leaves it `Canceled` because the task never runs its tail.
    pub fn submit<F>(&self, label: impl Into<String>, progress: Arc<ProgressLog>, fut: F) -> String
    where
        F: Future<Output = Result<JobResult, String>> + Send + 'static,
    {
        self.submit_as(self.local_id(), label, progress, fut)
    }

    /// [`submit`](Self::submit) under an id from [`mint`](Self::mint).
    pub fn submit_as<F>(
        &self,
        id: String,
        label: impl Into<String>,
        progress: Arc<ProgressLog>,
        fut: F,
    ) -> String
    where
        F: Future<Output = Result<JobResult, String>> + Send + 'static,
    {
        let ledger = self.ledger.clone();
        let ledger_id = job_number(&id);
        if ledger.is_some() {
            self.ensure_heartbeat();
        }

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
            // Land the result in memory under the state lock, which is released before any
            // await. Only land it if a `job_cancel` didn't race in while we were finishing:
            // a caller who asked to cancel gets `Canceled`, not a surprise late answer.
            let landed = {
                let mut s = task_state.lock().expect("job state mutex poisoned");
                if matches!(*s, JobState::Running) {
                    let succeeded = outcome.is_ok();
                    let record = match &outcome {
                        Ok(result) => (JobStatus::Done, result.answer.clone()),
                        Err(text) => (JobStatus::Failed, text.clone()),
                    };
                    *s = match outcome {
                        Ok(result) => JobState::Done(result),
                        Err(text) => JobState::Failed(text),
                    };
                    Some((succeeded, record))
                } else {
                    None
                }
            };
            if let Some((succeeded, (status, text))) = landed {
                // The ledger write comes before the completion ping, so a collector the
                // ping wakes can already read the answer from the state db.
                if let (Some(ledger), Some(n)) = (&ledger, ledger_id) {
                    match ledger
                        .store
                        .job_finish(n, &ledger.owner, status, Some(&text))
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => tracing::warn!(target: "kaibo::jobs", job = %task_id, "job ledger row was already finished or canceled; the answer is in memory only"),
                        Err(e) => tracing::warn!(target: "kaibo::jobs", job = %task_id, error = %e, "job ledger write failed; the answer is in memory only"),
                    }
                }
                // Completion signal at **Warn** — kaibo's "the calling model should
                // see this" level (not severity): a finished/failed job is exactly
                // what a `job_wait` drains by default, and the `mcp_log` bridge also
                // mirrors it to a watching client. Still advisory — no MCP primitive
                // wakes the agent, so polling/`job_wait` stays the contract. (A canceled
                // job never reaches here — its task is aborted — no ping for it.)
                if succeeded {
                    tracing::warn!(target: "kaibo::jobs", job = %task_id, "async job finished — collect it with `job_get`");
                } else {
                    tracing::warn!(target: "kaibo::jobs", job = %task_id, "async job failed — `job_get` it for the reason");
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
            progress,
        });
        self.lock().put(id.clone(), job);
        id
    }

    /// Snapshot a job's state for rendering — `None` if the id is unknown (never
    /// existed, or evicted). Touching the job promotes it to most-recently-used.
    pub fn get(&self, id: &str) -> Option<JobSnapshot> {
        let job = self.lock().get(id).cloned()?;
        Some(Self::snapshot(&job))
    }

    /// Render one job to a snapshot: clone its terminal-or-running state and, while it's
    /// still running, the latest progress beat. A finished job carries its answer, not a
    /// beat — the recorder's last line is moot once `state` is terminal, so we only attach
    /// it for `Running`.
    fn snapshot(job: &Job) -> JobSnapshot {
        let state = job.state.lock().expect("job state mutex poisoned").clone();
        let running = matches!(state, JobState::Running);
        let last_progress = running.then(|| job.progress.latest()).flatten();
        let chat = running.then(|| job.progress.chat_summary()).flatten();
        JobSnapshot {
            state,
            label: job.label.clone(),
            age: job.started.elapsed(),
            last_progress,
            chat,
        }
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
    /// `job_list` shows them. A read: it promotes nothing and takes no job's state lock for
    /// longer than the clone.
    pub fn list(&self) -> Vec<(String, JobSnapshot)> {
        self.lock()
            .iter()
            .map(|(id, job)| (id.clone(), Self::snapshot(job)))
            .collect()
    }

    /// Start the heartbeat task once: every [`HEARTBEAT_EVERY_SECS`] it refreshes the ledger
    /// rows of the jobs this process still runs, stops any job another process asked to
    /// cancel, and marks rows whose owner is gone as interrupted. It holds only a weak
    /// reference, so it ends when the store is dropped.
    fn ensure_heartbeat(&self) {
        let Some(ledger) = self.ledger.clone() else {
            return;
        };
        if self.heartbeat_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let next_id = self.next_id.clone();
        let started = self.heartbeat_started.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_EVERY_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                JobStore {
                    inner,
                    next_id: next_id.clone(),
                    ledger: Some(ledger.clone()),
                    heartbeat_started: started.clone(),
                }
                .heartbeat_once()
                .await;
            }
        });
    }

    /// One heartbeat: refresh the rows of the jobs this process still runs, stop the ones
    /// another process asked to cancel, and mark rows with no owner as interrupted.
    pub async fn heartbeat_once(&self) {
        let Some(ledger) = &self.ledger else { return };
        let numbers: Vec<i64> = self
            .lock()
            .iter()
            .filter(|(_, job)| {
                matches!(
                    *job.state.lock().expect("job state mutex poisoned"),
                    JobState::Running
                )
            })
            .filter_map(|(id, _)| job_number(id))
            .collect();
        match ledger.store.job_heartbeat(&ledger.owner, &numbers).await {
            Ok(cancel) => {
                for n in cancel {
                    let id = format!("job-{n}");
                    if self.cancel(&id) == CancelOutcome::Canceled {
                        self.record_canceled(&id).await;
                    }
                }
            }
            Err(e) => tracing::warn!(target: "kaibo::jobs", error = %e, "job heartbeat failed"),
        }
        if let Err(e) = ledger.store.job_sweep_stale().await {
            tracing::warn!(target: "kaibo::jobs", error = %e, "job sweep failed");
        }
    }

    /// A job by id: this process first, then the ledger (a job from before a restart, or
    /// one another process runs). `None` when neither knows it.
    pub async fn lookup(&self, id: &str) -> Option<JobSnapshot> {
        if let Some(snap) = self.get(id) {
            return Some(snap);
        }
        let ledger = self.ledger.as_ref()?;
        let n = job_number(id)?;
        if let Err(e) = ledger.store.job_sweep_stale().await {
            tracing::warn!(target: "kaibo::jobs", error = %e, "job sweep failed");
        }
        match ledger.store.job_get(n).await {
            Ok(row) => row.map(|r| snapshot_of_row(&r)),
            Err(e) => {
                tracing::warn!(target: "kaibo::jobs", error = %e, "job ledger read failed");
                None
            }
        }
    }

    /// Every job: this process's, then ledger rows it does not hold, newest first.
    pub async fn list_all(&self) -> Vec<(String, JobSnapshot)> {
        let mut out = self.list();
        let Some(ledger) = &self.ledger else {
            return out;
        };
        if let Err(e) = ledger.store.job_sweep_stale().await {
            tracing::warn!(target: "kaibo::jobs", error = %e, "job sweep failed");
        }
        match ledger.store.job_list().await {
            Ok(rows) => {
                for row in rows {
                    let id = format!("job-{}", row.id);
                    if !out.iter().any(|(held, _)| *held == id) {
                        out.push((id, snapshot_of_row(&row)));
                    }
                }
            }
            Err(e) => tracing::warn!(target: "kaibo::jobs", error = %e, "job ledger read failed"),
        }
        // Newest first by number; ids without one (`job-local-N`) keep their place after.
        out.sort_by_key(|(id, _)| std::cmp::Reverse(job_number(id).unwrap_or(-1)));
        out
    }

    /// Cancel a job by id: stop it here when this process runs it, record a request when
    /// another process does.
    pub async fn cancel_any(&self, id: &str) -> CancelOutcome {
        let local = self.cancel(id);
        if local != CancelOutcome::Unknown {
            if local == CancelOutcome::Canceled {
                self.record_canceled(id).await;
            }
            return local;
        }
        let (Some(ledger), Some(n)) = (&self.ledger, job_number(id)) else {
            return CancelOutcome::Unknown;
        };
        match ledger.store.job_get(n).await {
            // This process's own row with no task behind it: the task was evicted and
            // aborted, so nothing will ever read a cancel request. Settle the row here.
            Ok(Some(row)) if row.status == JobStatus::Running && *row.owner == *ledger.owner => {
                self.record_canceled(id).await;
                CancelOutcome::Canceled
            }
            Ok(Some(row)) if row.status == JobStatus::Running => {
                match ledger.store.job_request_cancel(n).await {
                    Ok(true) => CancelOutcome::Requested,
                    Ok(false) => CancelOutcome::AlreadyFinished,
                    Err(e) => {
                        tracing::warn!(target: "kaibo::jobs", error = %e, "job ledger write failed");
                        CancelOutcome::Unknown
                    }
                }
            }
            Ok(Some(row)) if row.status == JobStatus::Canceled => CancelOutcome::Canceled,
            Ok(Some(_)) => CancelOutcome::AlreadyFinished,
            _ => CancelOutcome::Unknown,
        }
    }

    async fn record_canceled(&self, id: &str) {
        let (Some(ledger), Some(n)) = (&self.ledger, job_number(id)) else {
            return;
        };
        if let Err(e) = ledger
            .store
            .job_finish(n, &ledger.owner, JobStatus::Canceled, None)
            .await {
            tracing::warn!(target: "kaibo::jobs", error = %e, "job ledger write failed");
        }
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

/// A ledger row as a snapshot. A running row has no progress log here: the task is in
/// another process, or in none.
fn snapshot_of_row(row: &JobRow) -> JobSnapshot {
    let state = match (row.status, &row.result) {
        (JobStatus::Running, _) => JobState::Running,
        (JobStatus::Done, answer) => JobState::Done(JobResult {
            answer: answer.clone().unwrap_or_default(),
        }),
        (JobStatus::Failed, text) => JobState::Failed(text.clone().unwrap_or_default()),
        (JobStatus::Canceled, _) => JobState::Canceled,
        (JobStatus::Interrupted, _) => JobState::Failed(
            "the kaibo process that ran this job ended before it finished, so there is no \
             answer. Submit the question again."
                .to_string(),
        ),
    };
    let end = row.finished_at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    JobSnapshot {
        state,
        label: row.label.clone(),
        age: Duration::from_secs((end - row.created_at).max(0) as u64),
        last_progress: None,
        chat: None,
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
        }
    }

    /// A record-only progress log for tests that don't exercise the beat echo — the
    /// store needs one per `submit`, but most lifecycle tests only care about state.
    fn pl() -> Arc<ProgressLog> {
        Arc::new(ProgressLog::silent())
    }

    /// Poll `job_get` until the job leaves `Running`, or panic after a generous bound — the
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
        let id = store.submit("cast `x`", pl(), async { Ok(done("the answer")) });
        assert_eq!(
            await_terminal(&store, &id).await,
            JobState::Done(done("the answer"))
        );
    }

    #[tokio::test]
    async fn a_failing_future_lands_as_failed() {
        let store = JobStore::new(cap(4));
        let id = store.submit("cast `x`", pl(), async {
            Err("provider exploded".to_string())
        });
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
        let id = store.submit("cast `x`", pl(), async move {
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
        let a = store.submit("a", pl(), async { Ok(done("a")) });
        let b = store.submit("b", pl(), async { Ok(done("b")) });
        // Touch `a` so `b` becomes the least-recently-used.
        let _ = store.get(&a);
        let c = store.submit("c", pl(), async { Ok(done("c")) });

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
        let id = store.submit("cast `x`", pl(), async move {
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
        let id = store.submit("cast `x`", pl(), async { Ok(done("fast")) });
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
        let _evicted = store.submit("a", pl(), async move {
            let _ = rx.await;
            flag.store(true, AOrd::SeqCst);
            Ok(done("a"))
        });
        // A second submit at cap 1 evicts the first → its `Job` drops → task aborts.
        let _b = store.submit("b", pl(), async { Ok(done("b")) });
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
        let id = store.submit("cast `x`", pl(), async {
            panic!("boom in the consult loop")
        });
        assert!(
            matches!(await_terminal(&store, &id).await, JobState::Failed(_)),
            "a panicking task must land as Failed, not hang in Running forever"
        );
    }

    #[tokio::test]
    async fn a_running_jobs_snapshot_echoes_the_latest_progress_beat() {
        use crate::progress::{PhaseEvent, ProgressSink};
        let store = JobStore::new(cap(4));
        // Hold the progress log so the test can drive beats through it, the way the real
        // consult loop emits into the `ConsultConfig.progress` clone the job shares.
        let progress = Arc::new(ProgressLog::silent());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = store.submit("cast `x`", progress.clone(), async move {
            let _ = rx.await;
            Ok(done("eventually"))
        });

        // Before any beat, a running job has nothing to echo.
        assert_eq!(store.get(&id).unwrap().last_progress, None);

        // A beat fires → the snapshot surfaces it (message + count) while still Running.
        progress.emit(PhaseEvent::KaishRun {
            script: "grep -rn sandbox src".into(),
        });
        assert_eq!(
            store.get(&id).unwrap().last_progress,
            Some(("running kaish: grep -rn sandbox src".to_string(), 1))
        );

        // Once it finishes, the beat is moot — a Done snapshot carries the answer, not a
        // stale "currently …" line.
        tx.send(()).unwrap();
        assert_eq!(
            await_terminal(&store, &id).await,
            JobState::Done(done("eventually"))
        );
        assert_eq!(store.get(&id).unwrap().last_progress, None);
    }

    /// The latency clause follows the same rule as the beat: echoed while the job runs,
    /// dropped once it is terminal (the answer is what a finished job carries).
    #[tokio::test]
    async fn a_running_jobs_snapshot_echoes_the_chat_latency_and_a_finished_one_does_not() {
        use crate::progress::{PhaseEvent, ProgressSink};
        let store = JobStore::new(cap(4));
        let progress = Arc::new(ProgressLog::silent());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = store.submit("cast `x`", progress.clone(), async move {
            let _ = rx.await;
            Ok(done("eventually"))
        });
        assert_eq!(store.get(&id).unwrap().chat, None, "no call yet");

        progress.emit(PhaseEvent::ChatCompleted {
            agent: "synth",
            elapsed: Duration::from_secs(107),
        });
        assert_eq!(
            store.get(&id).unwrap().chat.as_deref(),
            Some("synth chat: last 107 s, p50 107 s over 1 call")
        );

        tx.send(()).unwrap();
        await_terminal(&store, &id).await;
        assert_eq!(store.get(&id).unwrap().chat, None);
    }

    #[tokio::test]
    async fn list_returns_every_live_job_newest_first() {
        let store = JobStore::new(cap(4));
        let a = store.submit("cast `a`", pl(), async { Ok(done("a")) });
        let b = store.submit("cast `b`", pl(), async { Ok(done("b")) });
        let listed: Vec<String> = store.list().into_iter().map(|(id, _)| id).collect();
        // Most-recently-submitted first: b before a.
        assert_eq!(listed, vec![b, a]);
    }

    #[tokio::test]
    async fn ids_are_unique_and_monotonic() {
        let store = JobStore::new(cap(8));
        let a = store.submit("a", pl(), async { Ok(done("a")) });
        let b = store.submit("b", pl(), async { Ok(done("b")) });
        assert_ne!(a, b);
        assert_eq!(a, "job-1");
        assert_eq!(b, "job-2");
    }
}
