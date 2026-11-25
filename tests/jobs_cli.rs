//! `kaibo jobs` against a real state db. The test process writes the ledger and the
//! spawned `kaibo` binary reads it, so this is two real processes on one db file.

#![cfg(unix)] // two processes share one db file only where the store runs multi-process

use std::num::NonZeroUsize;
use std::process::{Command, Output};

use kaibo::store::jobs::JobStatus;
use kaibo::store::SessionStore;
use tempfile::TempDir;

struct Env {
    home: TempDir,
    project: TempDir,
    store: SessionStore,
}

impl Env {
    async fn new() -> Self {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let db = home.path().join("kaibo").join("state.db");
        let store = SessionStore::open(&db, NonZeroUsize::new(8).unwrap(), &[])
            .await
            .expect("open the state db");
        Self {
            home,
            project,
            store,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kaibo"))
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path())
            .env("XDG_DATA_HOME", self.home.path())
            .current_dir(self.project.path())
            .arg("jobs")
            .args(args)
            .output()
            .unwrap()
    }

    async fn done(&self, answer: &str) -> String {
        let n = self
            .store
            .job_create("consult", "cast `x`", "o")
            .await
            .unwrap();
        self.store
            .job_finish(n, "o", JobStatus::Done, Some(answer))
            .await
            .unwrap();
        format!("job-{n}")
    }

    async fn failed(&self, text: &str) -> String {
        let n = self
            .store
            .job_create("consult", "cast `x`", "o")
            .await
            .unwrap();
        self.store
            .job_finish(n, "o", JobStatus::Failed, Some(text))
            .await
            .unwrap();
        format!("job-{n}")
    }

    async fn running(&self) -> String {
        let n = self
            .store
            .job_create("consult", "cast `x`", "live")
            .await
            .unwrap();
        format!("job-{n}")
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[tokio::test]
async fn list_says_so_when_there_are_no_jobs() {
    let env = Env::new().await;
    let o = env.run(&["list"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("No background jobs."), "{}", out(&o));
}

#[tokio::test]
async fn list_shows_jobs_another_process_wrote_newest_first() {
    let env = Env::new().await;
    let a = env.done("first").await;
    let b = env.running().await;
    let o = env.run(&["list"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    let (pb, pa) = (text.find(&b).unwrap(), text.find(&a).unwrap());
    assert!(pb < pa, "newest first: {text}");
    assert!(text.contains("running") && text.contains("done"), "{text}");

    let j = env.run(&["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&out(&j)).unwrap();
    assert_eq!(rows[0]["id"], b);
    assert_eq!(rows[0]["status"], "running");
    assert_eq!(rows[0]["label"], "cast `x`");
    assert!(rows[0]["age_secs"].is_u64(), "age_secs is a number: {rows}");
    assert_eq!(rows[1]["status"], "done");
}

#[tokio::test]
async fn get_prints_the_answer_and_exits_by_outcome() {
    let env = Env::new().await;
    let done = env.done("the stored answer").await;
    let failed = env.failed("provider exploded").await;
    let running = env.running().await;

    let o = env.run(&["get", &done]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(out(&o).trim(), "the stored answer");

    let o = env.run(&["get", &failed]);
    assert_eq!(o.status.code(), Some(4), "a failed job exits 4");
    assert!(out(&o).contains("provider exploded"));

    let o = env.run(&["get", &running]);
    assert_eq!(o.status.code(), Some(0), "a running job is not a failure");
    assert!(out(&o).contains("still running"), "{}", out(&o));
    assert!(out(&o).contains("kaibo jobs wait"), "names the next step");

    let o = env.run(&["get", "--json", &done]);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["status"], "done");
    assert_eq!(v["text"], "the stored answer");
    assert_eq!(v["id"], done);
    assert_eq!(v["label"], "cast `x`");
    assert!(v["age_secs"].is_u64());
}

#[tokio::test]
async fn wait_returns_when_jobs_are_done_and_times_out_on_a_running_one() {
    let env = Env::new().await;
    let a = env.done("answer a").await;
    let b = env.done("answer b").await;
    let o = env.run(&["wait", &a, &b]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    assert!(
        text.contains(&format!("== {a} (done) ==")) && text.contains(&format!("== {b} (done) ==")),
        "each job has its own header: {text}"
    );
    assert!(
        text.contains("answer a") && text.contains("answer b"),
        "{text}"
    );

    let running = env.running().await;
    let o = env.run(&["wait", "--timeout", "1", &running]);
    assert_eq!(o.status.code(), Some(4), "a timeout exits 4");
    assert!(err(&o).contains("still running after 1s"), "{}", err(&o));
    assert!(
        err(&o).contains("kaibo jobs wait"),
        "names how to wait again"
    );
}

#[tokio::test]
async fn wait_exits_4_when_any_job_failed() {
    let env = Env::new().await;
    let ok = env.done("fine").await;
    let bad = env.failed("broke").await;
    let o = env.run(&["wait", &ok, &bad]);
    assert_eq!(o.status.code(), Some(4));
}

#[tokio::test]
async fn cancel_records_a_request_for_the_owner() {
    let env = Env::new().await;
    let running = env.running().await;
    let o = env.run(&["cancel", &running]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("Asked the process"), "{}", out(&o));
    let n: i64 = running.trim_start_matches("job-").parse().unwrap();
    assert!(
        env.store
            .job_get(n)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested,
        "the owner reads this at its next heartbeat"
    );

    let done = env.done("x").await;
    let o = env.run(&["cancel", &done]);
    assert_eq!(o.status.code(), Some(0));
    assert!(out(&o).contains("already ended"), "{}", out(&o));
}

#[tokio::test]
async fn unknown_and_batch_handles_are_refused_with_the_fix() {
    let env = Env::new().await;
    let o = env.run(&["get", "job-404"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("kaibo jobs list"), "{}", err(&o));

    let o = env.run(&["get", "anthropic/msgbatch_1"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        err(&o).contains("kaibo batch get anthropic/msgbatch_1"),
        "{}",
        err(&o)
    );

    let o = env.run(&["get", "nonsense"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("job-12"), "shows the shape: {}", err(&o));
}

#[tokio::test]
async fn persistence_off_is_refused_and_says_how_to_turn_it_on() {
    let env = Env::new().await;
    let o = env.run(&["--no-persistence", "list"]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("[persistence] enabled"), "{}", err(&o));
}

#[tokio::test]
async fn wait_json_reports_every_job_and_a_timeout() {
    let env = Env::new().await;
    let a = env.done("answer a").await;
    let o = env.run(&["wait", "--json", &a]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let rows: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(rows[0]["id"], a);
    assert_eq!(rows[0]["status"], "done");
    assert_eq!(rows[0]["text"], "answer a");

    let running = env.running().await;
    let o = env.run(&["wait", "--json", "--timeout", "1", &running]);
    assert_eq!(o.status.code(), Some(4));
    let rows: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        rows[0]["status"], "running",
        "a timeout still reports where each job is"
    );
}

#[tokio::test]
async fn cancel_json_has_its_own_shape() {
    let env = Env::new().await;
    let running = env.running().await;
    let o = env.run(&["cancel", "--json", &running]);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["id"], running);
    assert_eq!(v["status"], "requested");
    assert!(v["message"].as_str().unwrap().contains("Asked the process"));

    let done = env.done("x").await;
    let o = env.run(&["cancel", "--json", &done]);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["status"], "already_ended");
}

#[tokio::test]
async fn an_interrupted_job_reads_as_a_failure_that_says_to_submit_again() {
    let env = Env::new().await;
    let n = env
        .store
        .job_create("consult", "cast `x`", "dead")
        .await
        .unwrap();
    // The owner stopped heartbeating long ago.
    let far = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3600;
    env.store.job_sweep_at(far).await.unwrap();
    let o = env.run(&["get", &format!("job-{n}")]);
    assert_eq!(o.status.code(), Some(4), "{}", out(&o));
    assert!(out(&o).contains("Submit the question again"), "{}", out(&o));
}

#[tokio::test]
async fn a_bad_handle_is_a_usage_error_even_when_the_db_is_unavailable() {
    let env = Env::new().await;
    let o = env.run(&["--no-persistence", "get", "nonsense"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "the handle is checked before the state db opens: {}",
        err(&o)
    );
    let o = env.run(&["get", "job-local-3"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("job_get"), "{}", err(&o));
}

#[tokio::test]
async fn an_absurd_timeout_is_a_usage_error_not_a_panic() {
    let env = Env::new().await;
    let done = env.done("x").await;
    let o = env.run(&["wait", "--timeout", "18446744073709551615", &done]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
}

#[tokio::test]
async fn the_timeout_is_honored_to_the_second() {
    let env = Env::new().await;
    let running = env.running().await;
    let started = std::time::Instant::now();
    let o = env.run(&["wait", "--timeout", "1", &running]);
    assert_eq!(o.status.code(), Some(4));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1900),
        "a 1s wait must not sleep a whole 2s poll: {:?}",
        started.elapsed()
    );
}
