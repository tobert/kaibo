//! The job ledger: jobs recorded in the state db, read back by a second `JobStore` that
//! stands in for another kaibo process (same db, its own owner token and empty memory).

use std::num::NonZeroUsize;
use std::sync::Arc;

use kaibo::jobs::{CancelOutcome, JobResult, JobState, JobStore};
use kaibo::progress::ProgressLog;
use kaibo::store::jobs::{JobStatus, RETENTION_SECS, STALE_AFTER_SECS};
use kaibo::store::SessionStore;
use tempfile::TempDir;

fn cap() -> NonZeroUsize {
    NonZeroUsize::new(16).unwrap()
}

async fn open() -> (SessionStore, TempDir) {
    let dir = TempDir::new().unwrap();
    let store = SessionStore::open(&dir.path().join("state.db"), cap(), &[])
        .await
        .expect("open store");
    (store, dir)
}

fn pl() -> Arc<ProgressLog> {
    Arc::new(ProgressLog::silent())
}

fn answer(text: &str) -> JobResult {
    JobResult {
        answer: text.to_string(),
    }
}

async fn submit_done(jobs: &JobStore, text: &'static str) -> String {
    let id = jobs.mint("consult", "cast `x`").await;
    jobs.submit_as(
        id.clone(),
        "cast `x`",
        pl(),
        async move { Ok(answer(text)) },
    );
    id
}

async fn until_done(jobs: &JobStore, id: &str) -> JobState {
    for _ in 0..2000 {
        match jobs.lookup(id).await.map(|s| s.state) {
            Some(JobState::Running) | None => tokio::task::yield_now().await,
            Some(done) => return done,
        }
    }
    panic!("job {id} never finished");
}

#[tokio::test]
async fn a_finished_job_is_readable_by_another_process() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let reader = JobStore::new(cap()).with_ledger(store);

    let id = submit_done(&owner, "the answer").await;
    assert_eq!(
        until_done(&owner, &id).await,
        JobState::Done(answer("the answer"))
    );
    // The reader holds nothing in memory: only the ledger can answer.
    assert!(reader.get(&id).is_none());
    assert_eq!(
        until_done(&reader, &id).await,
        JobState::Done(answer("the answer"))
    );
}

#[tokio::test]
async fn a_failed_job_keeps_its_failure_text() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let reader = JobStore::new(cap()).with_ledger(store);

    let id = owner.mint("consult", "x").await;
    owner.submit_as(id.clone(), "x", pl(), async {
        Err("provider exploded".to_string())
    });
    until_done(&owner, &id).await;
    assert_eq!(
        until_done(&reader, &id).await,
        JobState::Failed("provider exploded".to_string())
    );
}

#[tokio::test]
async fn ids_never_collide_across_processes_or_restarts() {
    let (store, _d) = open().await;
    let a = JobStore::new(cap()).with_ledger(store.clone());
    let b = JobStore::new(cap()).with_ledger(store.clone());
    let mut ids = vec![
        a.mint("consult", "x").await,
        b.mint("consult", "x").await,
        a.mint("consult", "x").await,
    ];
    // A "restarted" process: new memory, same db.
    let restarted = JobStore::new(cap()).with_ledger(store);
    ids.push(restarted.mint("consult", "x").await);
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "duplicate job ids in {ids:?}");
}

#[tokio::test]
async fn list_all_shows_jobs_this_process_does_not_hold() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let reader = JobStore::new(cap()).with_ledger(store);
    let id = submit_done(&owner, "a").await;
    until_done(&owner, &id).await;

    assert!(
        reader.list().is_empty(),
        "memory-only list stays process-local"
    );
    let all = reader.list_all().await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, id);
}

#[tokio::test]
async fn a_job_whose_owner_stopped_heartbeating_reads_as_interrupted() {
    let (store, _d) = open().await;
    let id = store
        .job_create("consult", "x", "dead-owner")
        .await
        .unwrap();
    let reader = JobStore::new(cap()).with_ledger(store.clone());

    // Fresh heartbeat: still running.
    assert_eq!(
        reader.lookup(&format!("job-{id}")).await.map(|s| s.state),
        Some(JobState::Running)
    );
    // The same row seen after the stale window.
    let marked = store
        .job_sweep_at(kaibo_now() + STALE_AFTER_SECS + 1)
        .await
        .unwrap();
    assert_eq!(marked, 1);
    let row = store.job_get(id).await.unwrap().unwrap();
    assert_eq!(row.status, JobStatus::Interrupted);
    match reader.lookup(&format!("job-{id}")).await.unwrap().state {
        JobState::Failed(text) => assert!(
            text.contains("Submit the question again"),
            "names what to do: {text}"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_running_job_survives_a_sweep_inside_the_stale_window() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let id = owner.mint("consult", "x").await;
    owner.submit_as(id.clone(), "x", pl(), async move {
        let _ = rx.await;
        Ok(answer("late"))
    });
    // A sweep inside the stale window leaves a running job alone.
    let n: i64 = id.trim_start_matches("job-").parse().unwrap();
    owner.heartbeat_once().await;
    let swept = store
        .job_sweep_at(kaibo_now() + STALE_AFTER_SECS - 5)
        .await
        .unwrap();
    assert_eq!(swept, 0);
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Running
    );
    tx.send(()).unwrap();
    assert_eq!(
        until_done(&owner, &id).await,
        JobState::Done(answer("late"))
    );
}

#[tokio::test]
async fn cancel_from_another_process_is_honored_at_the_next_heartbeat() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let other = JobStore::new(cap()).with_ledger(store.clone());
    let id = owner.mint("consult", "x").await;
    owner.submit_as(id.clone(), "x", pl(), async {
        std::future::pending::<Result<JobResult, String>>().await
    });

    assert_eq!(other.cancel_any(&id).await, CancelOutcome::Requested);
    assert_eq!(
        owner.lookup(&id).await.map(|s| s.state),
        Some(JobState::Running),
        "nothing stops until the owner heartbeats"
    );
    owner.heartbeat_once().await;
    assert_eq!(
        owner.lookup(&id).await.map(|s| s.state),
        Some(JobState::Canceled)
    );
    assert_eq!(
        other.lookup(&id).await.map(|s| s.state),
        Some(JobState::Canceled),
        "the ledger records the cancel"
    );
}

#[tokio::test]
async fn a_late_result_cannot_overwrite_canceled() {
    let (store, _d) = open().await;
    let n = store.job_create("consult", "x", "o").await.unwrap();
    assert!(store
        .job_finish(n, "o", JobStatus::Canceled, None)
        .await
        .unwrap());
    assert!(!store
        .job_finish(n, "o", JobStatus::Done, Some("too late"))
        .await
        .unwrap());
    let row = store.job_get(n).await.unwrap().unwrap();
    assert_eq!(row.status, JobStatus::Canceled);
    assert_eq!(row.result, None);
}

#[tokio::test]
async fn finished_jobs_past_retention_are_deleted_and_their_numbers_do_not_return() {
    let (store, _d) = open().await;
    let old = store.job_create("consult", "x", "o").await.unwrap();
    store
        .job_finish(old, "o", JobStatus::Done, Some("a"))
        .await
        .unwrap();
    let running = store.job_create("consult", "x", "o").await.unwrap();

    store
        .job_sweep_at(kaibo_now() + RETENTION_SECS + 1)
        .await
        .unwrap();
    assert!(
        store.job_get(old).await.unwrap().is_none(),
        "old finished row deleted"
    );
    assert_eq!(
        store.job_get(running).await.unwrap().unwrap().status,
        JobStatus::Interrupted,
        "an owner-less running row is marked, and stays for a full retention period"
    );
    let next = store.job_create("consult", "x", "o").await.unwrap();
    assert!(next > running, "ids keep counting after a prune");
}

#[tokio::test]
async fn without_a_ledger_jobs_stay_in_memory() {
    let jobs = JobStore::new(cap());
    assert!(!jobs.is_durable());
    let id = submit_done(&jobs, "a").await;
    assert_eq!(until_done(&jobs, &id).await, JobState::Done(answer("a")));
    assert_eq!(jobs.cancel_any("job-999").await, CancelOutcome::Unknown);
}

fn kaibo_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn an_owner_that_was_swept_while_alive_still_lands_its_answer() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let id = owner.mint("consult", "x").await;
    owner.submit_as(id.clone(), "x", pl(), async move {
        let _ = rx.await;
        Ok(answer("slept through the sweep"))
    });
    let n: i64 = id.trim_start_matches("job-").parse().unwrap();

    // Another process sweeps after the owner's heartbeat lapsed (a sleeping laptop).
    store
        .job_sweep_at(kaibo_now() + STALE_AFTER_SECS + 1)
        .await
        .unwrap();
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Interrupted
    );

    tx.send(()).unwrap();
    until_done(&owner, &id).await;
    let reader = JobStore::new(cap()).with_ledger(store);
    // The ledger write follows the in-memory landing by a tick.
    let mut landed = None;
    for _ in 0..2000 {
        match reader.lookup(&id).await.map(|s| s.state) {
            Some(JobState::Done(r)) => {
                landed = Some(r);
                break;
            }
            _ => tokio::task::yield_now().await,
        }
    }
    assert_eq!(
        landed,
        Some(answer("slept through the sweep")),
        "the owner's answer must replace the interrupted mark"
    );
}

#[tokio::test]
async fn a_heartbeat_revives_a_row_swept_while_its_owner_was_alive() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let id = owner.mint("consult", "x").await;
    owner.submit_as(id.clone(), "x", pl(), async {
        std::future::pending::<Result<JobResult, String>>().await
    });
    let n: i64 = id.trim_start_matches("job-").parse().unwrap();
    store
        .job_sweep_at(kaibo_now() + STALE_AFTER_SECS + 1)
        .await
        .unwrap();
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Interrupted
    );
    owner.heartbeat_once().await;
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Running,
        "the owner's heartbeat takes the row back"
    );
}

#[tokio::test]
async fn a_process_cannot_finish_another_processs_row() {
    let (store, _d) = open().await;
    let n = store.job_create("consult", "x", "owner-a").await.unwrap();
    assert!(!store
        .job_finish(n, "owner-b", JobStatus::Done, Some("not mine"))
        .await
        .unwrap());
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Running
    );
}

#[tokio::test]
async fn a_process_local_id_never_addresses_a_ledger_row() {
    let (store, _d) = open().await;
    let other = store.job_create("consult", "x", "owner-a").await.unwrap();
    assert_eq!(other, 1);
    let jobs = JobStore::new(cap()).with_ledger(store.clone());
    // `submit` takes the process-local route even with a ledger attached.
    let local = jobs.submit("x", pl(), async { Ok(answer("mine")) });
    assert!(local.starts_with("job-local-"), "got {local}");
    until_done(&jobs, &local).await;
    assert_eq!(
        store.job_get(1).await.unwrap().unwrap().status,
        JobStatus::Running,
        "the other process's job-1 is untouched"
    );
}

#[tokio::test]
async fn cancel_on_a_job_that_just_finished_does_not_promise_a_stop() {
    let (store, _d) = open().await;
    let n = store.job_create("consult", "x", "owner-a").await.unwrap();
    store
        .job_finish(n, "owner-a", JobStatus::Done, Some("done"))
        .await
        .unwrap();
    assert!(!store.job_request_cancel(n).await.unwrap());
    let other = JobStore::new(cap()).with_ledger(store);
    assert_eq!(
        other.cancel_any(&format!("job-{n}")).await,
        CancelOutcome::AlreadyFinished
    );
}

#[tokio::test]
async fn cancel_of_this_processs_orphaned_row_settles_it_here() {
    let (store, _d) = open().await;
    let jobs = JobStore::new(cap()).with_ledger(store.clone());
    // A row this process minted whose task is gone (evicted and aborted).
    let id = jobs.mint("consult", "x").await;
    assert_eq!(jobs.cancel_any(&id).await, CancelOutcome::Canceled);
    let n: i64 = id.trim_start_matches("job-").parse().unwrap();
    assert_eq!(
        store.job_get(n).await.unwrap().unwrap().status,
        JobStatus::Canceled
    );
}

#[tokio::test]
async fn list_leaves_answers_out_and_merges_newest_first() {
    let (store, _d) = open().await;
    let owner = JobStore::new(cap()).with_ledger(store.clone());
    let a = submit_done(&owner, "first answer").await;
    let b = submit_done(&owner, "second answer").await;
    until_done(&owner, &a).await;
    until_done(&owner, &b).await;
    let rows = store.job_list().await.unwrap();
    assert!(
        rows.iter().all(|r| r.result.is_none()),
        "list carries no answers"
    );
    let reader = JobStore::new(cap()).with_ledger(store);
    let ids: Vec<String> = reader
        .list_all()
        .await
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    assert_eq!(ids, vec![b, a], "newest first");
}
