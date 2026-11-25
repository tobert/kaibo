//! `kaibo jobs` — look in on background jobs (`job-N`) from the shell.
//!
//! Reads the job ledger in the state db (`store/jobs.rs`) through the same
//! [`JobStore`] the MCP verbs use, so the CLI shows what any kaibo process started: a
//! `consult_submit` from an MCP session, a direct-lane `deliberate`, a deferred
//! `generate`. It needs no running server. It can read, wait for, and ask to cancel a job;
//! the task itself runs in the process that started it.

use std::time::Duration;

use super::{
    fail_preflight, init_cli_logging, load_config, CommonArgs, SetupError, EXIT_CONSULT_FAILURE,
    EXIT_OK, EXIT_SETUP, EXIT_USAGE,
};
use crate::jobs::{CancelOutcome, JobSnapshot, JobState, JobStore};
use crate::server::Resolver;
use clap::{Args, Subcommand};
use std::sync::Arc;

/// `kaibo jobs` — background jobs started by `consult_submit`, a direct `deliberate`, or a
/// deferred `generate`, from any kaibo process on this state db. Provider batches have
/// their own command, `kaibo batch`.
#[derive(Args, Debug)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub cmd: JobsCmd,
}

#[derive(Subcommand, Debug)]
pub enum JobsCmd {
    /// List jobs, newest first: status, how long each has run (or ran, once finished), and
    /// the cast that runs it.
    List(JobsListArgs),
    /// Print a job: its answer when done, a status line while it runs. Exits 4 for a job
    /// that failed, was canceled, or was interrupted.
    Get(JobsGetArgs),
    /// Wait until the jobs finish, then print each answer. Exits 0 when every job is done,
    /// 4 when one failed, was canceled, or was interrupted, or when `--timeout` passes.
    Wait(JobsWaitArgs),
    /// Ask the process that runs a job to stop it. If that process is alive, the job stops
    /// within about ten seconds; `get` shows what happened.
    Cancel(JobsCancelArgs),
}

#[derive(Args, Debug)]
pub struct JobsListArgs {
    /// Emit a JSON array of `{id, status, label, age_secs}` instead of prose. `age_secs` is
    /// how long the job has run, or ran if it finished.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JobsGetArgs {
    /// The `job-N` handle `consult_submit` returned.
    pub handle: String,

    /// Emit a JSON object `{id, status, label, age_secs, text}` instead of prose.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JobsCancelArgs {
    /// The `job-N` handle to stop.
    pub handle: String,

    /// Emit a JSON object `{id, status, message}` instead of prose. `status` is
    /// `requested`, `canceled`, or `already_ended`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JobsWaitArgs {
    /// One or more `job-N` handles.
    #[arg(required = true, value_name = "HANDLE")]
    pub handles: Vec<String>,

    /// Stop waiting after this many seconds (default 600, at most 604800). The jobs keep
    /// running.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 600,
        value_parser = clap::value_parser!(u64).range(..=604_800)
    )]
    pub timeout: u64,

    /// Emit a JSON array of `{id, status, label, age_secs, text}` instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// How often `wait` reads the ledger.
const POLL_EVERY: Duration = Duration::from_secs(2);

fn setup(kind: &'static str, message: String, code: i32) -> SetupError {
    SetupError {
        kind,
        message,
        code,
    }
}

/// The ledger-backed job view over the state db, or the reason there is none.
async fn open_jobs(resolver: &Resolver) -> Result<JobStore, SetupError> {
    let persistence = &resolver.config.persistence;
    if !persistence.enabled {
        return Err(setup(
            "setup",
            "background jobs are recorded in the state db, and persistence is off, so the \
             CLI has nothing to read. Set `[persistence] enabled = true` in config.toml \
             (or drop `--no-persistence` / `KAIBO_NO_PERSISTENCE`); jobs started while it \
             was off live only in the server that started them."
                .to_string(),
            EXIT_SETUP,
        ));
    }
    // Config load refuses `enabled` with no resolvable path, so a path is always here.
    let path = persistence
        .path
        .clone()
        .expect("config load guarantees a state-db path while persistence is on");
    let allowed = resolver.allowed_set();
    let refs: Vec<&std::path::Path> = allowed.iter().map(std::path::PathBuf::as_path).collect();
    let store =
        crate::store::SessionStore::open(&path, resolver.config.defaults.session_capacity, &refs)
            .await
            .map_err(|e| {
                let message = match e {
                    crate::store::StoreError::PathInAllowedTree(_) => {
                        crate::store::path_in_allowed_tree_hint(&path)
                    }
                    other => format!(
                        "could not open the state db at {}: {other}. If another kaibo holds it \
                 (Windows and 32-bit platforms allow one process), close that kaibo; \
                 otherwise `kaibo config` shows the path, and `--state-db FILE` or \
                 `KAIBO_STATE_DB` points kaibo at another one.",
                        path.display()
                    ),
                };
                setup("setup", message, EXIT_SETUP)
            })?;
    Ok(JobStore::new(resolver.config.defaults.job_capacity).with_ledger(store))
}

/// `job-12` only: a batch handle has a `/`, and anything else is a typo.
fn check_handle(handle: &str) -> Result<(), SetupError> {
    if handle.contains('/') {
        return Err(setup(
            "usage",
            format!(
                "`{handle}` is a provider batch handle, and `kaibo jobs` reads background \
                 jobs (`job-N`). Run `kaibo batch get {handle}` instead."
            ),
            EXIT_USAGE,
        ));
    }
    if handle.starts_with("job-local-") {
        return Err(setup(
            "usage",
            format!(
                "`{handle}` is a job that only the process that started it recorded, because \
                 that process could not write the state db. `kaibo jobs` cannot reach it; \
                 collect it with `job_get` from the same server session."
            ),
            EXIT_USAGE,
        ));
    }
    let numbered = handle
        .strip_prefix("job-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !numbered {
        return Err(setup(
            "usage",
            format!(
                "`{handle}` is not a job handle. Handles look like `job-12`; \
                 `kaibo jobs list` shows them."
            ),
            EXIT_USAGE,
        ));
    }
    Ok(())
}

fn status_word(state: &JobState) -> &'static str {
    match state {
        JobState::Running => "running",
        JobState::Done(_) => "done",
        JobState::Failed(_) => "failed",
        JobState::Canceled => "canceled",
    }
}

fn is_terminal(state: &JobState) -> bool {
    !matches!(state, JobState::Running)
}

fn age(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 90 * 60 => format!("{}m", s / 60),
        s if s < 48 * 3600 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// The text a finished job prints: its answer or its failure, and a line for the rest.
fn text_of(id: &str, snap: &JobSnapshot) -> String {
    match &snap.state {
        JobState::Done(result) => result.answer.clone(),
        JobState::Failed(text) => text.clone(),
        JobState::Canceled => format!("Job `{id}` was canceled."),
        JobState::Running => format!(
            "Job `{id}` is still running — {} ({} elapsed). \
             `kaibo jobs wait {id}` waits for it.",
            snap.label,
            age(snap.age.as_secs())
        ),
    }
}

fn json_of(id: &str, snap: &JobSnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "status": status_word(&snap.state),
        "label": snap.label,
        "age_secs": snap.age.as_secs(),
        "text": text_of(id, snap),
    })
}

fn exit_of(snap: &JobSnapshot) -> i32 {
    match snap.state {
        JobState::Running | JobState::Done(_) => EXIT_OK,
        JobState::Failed(_) | JobState::Canceled => EXIT_CONSULT_FAILURE,
    }
}

pub async fn run_jobs(common: CommonArgs, args: JobsArgs) -> i32 {
    init_cli_logging();
    let json = match &args.cmd {
        JobsCmd::List(a) => a.json,
        JobsCmd::Get(a) => a.json,
        JobsCmd::Cancel(a) => a.json,
        JobsCmd::Wait(a) => a.json,
    };
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            return fail_preflight(json, "config", format!("config error: {e:#}"), EXIT_USAGE)
        }
    };
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => return fail_preflight(json, "setup", format!("{e:#}"), EXIT_SETUP),
    };
    let outcome = async {
        match &args.cmd {
            JobsCmd::List(_) => {}
            JobsCmd::Get(a) => check_handle(&a.handle)?,
            JobsCmd::Cancel(a) => check_handle(&a.handle)?,
            JobsCmd::Wait(a) => a.handles.iter().try_for_each(|h| check_handle(h))?,
        }
        let jobs = open_jobs(&resolver).await?;
        match args.cmd {
            JobsCmd::List(a) => list(&jobs, a.json).await,
            JobsCmd::Get(a) => get(&jobs, &a).await,
            JobsCmd::Wait(a) => wait(&jobs, &a).await,
            JobsCmd::Cancel(a) => cancel(&jobs, &a).await,
        }
    }
    .await;
    match outcome {
        Ok(code) => code,
        Err(e) => fail_preflight(json, e.kind, e.message, e.code),
    }
}

async fn list(jobs: &JobStore, json: bool) -> Result<i32, SetupError> {
    let all = jobs
        .try_list_all()
        .await
        .map_err(|e| ledger_unreadable(&e))?;
    if json {
        let rows: Vec<_> = all
            .iter()
            .map(|(id, snap)| {
                serde_json::json!({
                    "id": id,
                    "status": status_word(&snap.state),
                    "label": snap.label,
                    "age_secs": snap.age.as_secs(),
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(rows));
    } else if all.is_empty() {
        println!("No background jobs.");
    } else {
        for (id, snap) in &all {
            println!(
                "{id}  {:<8}  {:>4}  {}",
                status_word(&snap.state),
                age(snap.age.as_secs()),
                snap.label
            );
        }
    }
    Ok(EXIT_OK)
}

fn ledger_unreadable(e: &crate::store::StoreError) -> SetupError {
    setup(
        "setup",
        format!(
            "could not read the job ledger in the state db: {e}. The jobs may be there; \
             try again, and run `kaibo config` to see which db file kaibo reads."
        ),
        EXIT_SETUP,
    )
}

async fn get(jobs: &JobStore, args: &JobsGetArgs) -> Result<i32, SetupError> {
    check_handle(&args.handle)?;
    let Some(snap) = jobs
        .try_lookup(&args.handle)
        .await
        .map_err(|e| ledger_unreadable(&e))?
    else {
        return Err(not_found(&args.handle, "read"));
    };
    if args.json {
        println!("{}", json_of(&args.handle, &snap));
    } else {
        println!("{}", text_of(&args.handle, &snap));
    }
    Ok(exit_of(&snap))
}

fn not_found(handle: &str, verb: &str) -> SetupError {
    setup(
        "usage",
        format!(
            "no background job `{handle}` to {verb}: it never existed, or it finished more \
             than 7 days ago and was removed. `kaibo jobs list` shows the jobs there are."
        ),
        EXIT_USAGE,
    )
}

async fn wait(jobs: &JobStore, args: &JobsWaitArgs) -> Result<i32, SetupError> {
    for h in &args.handles {
        check_handle(h)?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut last: Vec<JobSnapshot> = Vec::new();
    loop {
        last.clear();
        for h in &args.handles {
            match jobs
                .try_lookup(h)
                .await
                .map_err(|e| ledger_unreadable(&e))?
            {
                Some(snap) => last.push(snap),
                None => return Err(not_found(h, "wait for")),
            }
        }
        if last.iter().all(|s| is_terminal(&s.state)) {
            break;
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        tokio::time::sleep(POLL_EVERY.min(left)).await;
    }
    let pending = last.iter().filter(|s| !is_terminal(&s.state)).count();
    if args.json {
        let rows: Vec<_> = args
            .handles
            .iter()
            .zip(&last)
            .map(|(h, s)| json_of(h, s))
            .collect();
        println!("{}", serde_json::Value::Array(rows));
    } else {
        let many = args.handles.len() > 1;
        for (h, s) in args.handles.iter().zip(&last) {
            if many {
                println!("== {h} ({}) ==", status_word(&s.state));
            }
            println!("{}", text_of(h, s));
        }
    }
    if pending > 0 {
        if !args.json {
            eprintln!(
                "{pending} job(s) still running after {}s. They keep running; wait again \
                 with `kaibo jobs wait {}`.",
                args.timeout,
                args.handles.join(" ")
            );
        }
        return Ok(EXIT_CONSULT_FAILURE);
    }
    Ok(last
        .iter()
        .map(exit_of)
        .find(|c| *c != EXIT_OK)
        .unwrap_or(EXIT_OK))
}

async fn cancel(jobs: &JobStore, args: &JobsCancelArgs) -> Result<i32, SetupError> {
    check_handle(&args.handle)?;
    let outcome = jobs
        .try_cancel_any(&args.handle)
        .await
        .map_err(|e| ledger_unreadable(&e))?;
    let (status, message) = match outcome {
        CancelOutcome::Requested => (
            "requested",
            format!(
                "Asked the process that runs `{}` to stop it. If that process is alive, the \
                 job stops within about ten seconds; `kaibo jobs get {}` shows what happened.",
                args.handle, args.handle
            ),
        ),
        CancelOutcome::Canceled => ("canceled", format!("Job `{}` is canceled.", args.handle)),
        CancelOutcome::AlreadyFinished => (
            "already_ended",
            format!(
                "Job `{0}` had already ended. `kaibo jobs get {0}` shows how.",
                args.handle
            ),
        ),
        CancelOutcome::Unknown => return Err(not_found(&args.handle, "cancel")),
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({ "id": args.handle, "status": status, "message": message })
        );
    } else {
        println!("{message}");
    }
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_picks_the_unit_a_person_reads() {
        assert_eq!(age(5), "5s");
        assert_eq!(age(89), "89s");
        assert_eq!(age(90), "1m");
        assert_eq!(age(90 * 60 - 1), "89m");
        assert_eq!(age(90 * 60), "1h");
        assert_eq!(age(48 * 3600), "2d");
    }

    #[test]
    fn handles_must_look_like_job_n() {
        assert!(check_handle("job-12").is_ok());
        for bad in ["job-", "job-x", "12", "JOB-1", "job-1/2"] {
            assert!(check_handle(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(check_handle("a/b").unwrap_err().code, EXIT_USAGE);
        assert!(check_handle("job-local-2")
            .unwrap_err()
            .message
            .contains("job_get"));
    }

    #[test]
    fn exit_codes_follow_the_job_state() {
        let snap = |state| JobSnapshot {
            state,
            label: String::new(),
            age: Duration::from_secs(1),
            last_progress: None,
            chat: None,
        };
        assert_eq!(exit_of(&snap(JobState::Running)), EXIT_OK);
        let done = JobState::Done(crate::jobs::JobResult { answer: "a".into() });
        assert_eq!(exit_of(&snap(done)), EXIT_OK);
        assert_eq!(
            exit_of(&snap(JobState::Failed("x".into()))),
            EXIT_CONSULT_FAILURE
        );
        assert_eq!(exit_of(&snap(JobState::Canceled)), EXIT_CONSULT_FAILURE);
    }
}
