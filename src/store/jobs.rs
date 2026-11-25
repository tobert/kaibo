//! The job ledger: one row per background job (`job-N`) in the state db.
//!
//! The in-memory [`crate::jobs::JobStore`] still runs the tasks (a task, its progress
//! log, and its abort handle live in one process). This ledger is the record every other
//! process reads: a finished job's answer survives a restart, a second kaibo process or
//! the CLI can list and fetch it, and a job whose process died reads as `interrupted`
//! instead of `running` forever.
//!
//! **Liveness is a heartbeat, not a process id.** An owner refreshes `heartbeat_at` on
//! every job it still runs; a `running` row whose heartbeat is older than
//! [`STALE_AFTER_SECS`] has no owner, and [`SessionStore::job_sweep_stale`] marks it
//! `interrupted`. A pid can be reused and cannot be probed on every platform.
//!
//! **Ids come from one counter row**, advanced inside `BEGIN IMMEDIATE`, so two processes
//! never mint the same `job-N`, and pruning old rows never lets a number come back.

use turso::Value;

use super::{now, Result, SessionStore, StoreError};

/// How often an owner refreshes the heartbeat of the jobs it runs.
pub const HEARTBEAT_EVERY_SECS: u64 = 10;

/// A `running` row with a heartbeat older than this has lost its owner.
pub const STALE_AFTER_SECS: i64 = 45;

/// Finished jobs are deleted after this long (7 days), at open and at every sweep.
pub const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// Where a job is, as the ledger records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Canceled,
    /// The owner stopped heartbeating before the job finished.
    Interrupted,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            "interrupted" => Self::Interrupted,
            other => {
                return Err(StoreError::Corrupt(format!(
                    "jobs.status is `{other}`, not a known job status"
                )))
            }
        })
    }
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// The number in `job-N`.
    pub id: i64,
    /// The producer: `consult`, `deliberate`, or `generate`.
    pub kind: String,
    /// The cast and model summary a poll line shows.
    pub label: String,
    pub status: JobStatus,
    /// The rendered answer (`done`) or the rendered failure text (`failed`). `None` while
    /// running. An answer that already points into the media CAS stores that pointer, not
    /// the bytes.
    pub result: Option<String>,
    /// The process that runs the job: only that process heartbeats it.
    pub owner: String,
    pub created_at: i64,
    pub heartbeat_at: i64,
    pub finished_at: Option<i64>,
    /// Another process asked to stop this job; the owner honors it at its next heartbeat.
    pub cancel_requested: bool,
}

const COLUMNS: &str = "id, kind, label, status, result, owner, created_at, heartbeat_at, \
                       finished_at, cancel_requested";

fn row_to_job(row: &turso::Row) -> Result<JobRow> {
    let text = |i: usize, what: &str| -> Result<String> {
        match row.get_value(i)? {
            Value::Text(s) => Ok(s),
            other => Err(StoreError::Corrupt(format!(
                "jobs.{what} is {other:?}, not TEXT"
            ))),
        }
    };
    let int = |i: usize, what: &str| -> Result<i64> {
        match row.get_value(i)? {
            Value::Integer(n) => Ok(n),
            other => Err(StoreError::Corrupt(format!(
                "jobs.{what} is {other:?}, not an INTEGER"
            ))),
        }
    };
    let result = match row.get_value(4)? {
        Value::Null => None,
        Value::Text(s) => Some(s),
        other => {
            return Err(StoreError::Corrupt(format!(
                "jobs.result is {other:?}, not TEXT or NULL"
            )))
        }
    };
    let finished_at = match row.get_value(8)? {
        Value::Null => None,
        Value::Integer(n) => Some(n),
        other => {
            return Err(StoreError::Corrupt(format!(
                "jobs.finished_at is {other:?}, not an INTEGER or NULL"
            )))
        }
    };
    Ok(JobRow {
        id: int(0, "id")?,
        kind: text(1, "kind")?,
        label: text(2, "label")?,
        status: JobStatus::parse(&text(3, "status")?)?,
        result,
        owner: text(5, "owner")?,
        created_at: int(6, "created_at")?,
        heartbeat_at: int(7, "heartbeat_at")?,
        finished_at,
        cancel_requested: int(9, "cancel_requested")? != 0,
    })
}

impl SessionStore {
    /// The v3 schema: the job ledger and its id counter.
    pub(super) async fn apply_v3(conn: &turso::Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS jobs (
                id               INTEGER PRIMARY KEY,
                kind             TEXT    NOT NULL,
                label            TEXT    NOT NULL,
                status           TEXT    NOT NULL,
                result           TEXT,
                owner            TEXT    NOT NULL,
                created_at       INTEGER NOT NULL,
                heartbeat_at     INTEGER NOT NULL,
                finished_at      INTEGER,
                cancel_requested INTEGER NOT NULL DEFAULT 0
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);

            CREATE TABLE IF NOT EXISTS job_counter (
                slot INTEGER PRIMARY KEY,
                n    INTEGER NOT NULL
            ) STRICT;
            "#,
        )
        .await?;
        Ok(())
    }

    /// Mint the next `job-N` and record it as `running`, owned by `owner`.
    pub async fn job_create(&self, kind: &str, label: &str, owner: &str) -> Result<i64> {
        let conn = self.conn().await?;
        let ts = now();
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let created = async {
            conn.execute(
                "INSERT INTO job_counter (slot, n) VALUES (1, 1)
                 ON CONFLICT(slot) DO UPDATE SET n = n + 1",
                (),
            )
            .await?;
            let mut rows = conn
                .query("SELECT n FROM job_counter WHERE slot = 1", ())
                .await?;
            let id = match rows.next().await?.map(|r| r.get_value(0)).transpose()? {
                Some(Value::Integer(n)) => n,
                other => {
                    return Err(StoreError::Corrupt(format!(
                        "job_counter.n is {other:?}, not an INTEGER"
                    )))
                }
            };
            conn.execute(
                "INSERT INTO jobs (id, kind, label, status, owner, created_at, heartbeat_at)
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?5)",
                (
                    id,
                    kind.to_string(),
                    label.to_string(),
                    owner.to_string(),
                    ts,
                ),
            )
            .await?;
            Ok(id)
        }
        .await;
        match created {
            Ok(id) => {
                conn.execute("COMMIT", ()).await?;
                Ok(id)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(e)
            }
        }
    }

    /// Land a job's outcome, as the job's `owner`. A `running` row changes, and so does an
    /// `interrupted` one: another process may sweep a live job while its owner sleeps, and
    /// the owner's answer still belongs on the row. A `canceled` or finished row never
    /// changes, and a process cannot write another process's row. Returns whether this
    /// call changed it.
    pub async fn job_finish(
        &self,
        id: i64,
        owner: &str,
        status: JobStatus,
        result: Option<&str>,
    ) -> Result<bool> {
        debug_assert!(
            status != JobStatus::Running,
            "finish needs a terminal status"
        );
        let conn = self.conn().await?;
        let result = match result {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        let changed = conn
            .execute(
                "UPDATE jobs SET status = ?2, result = ?3, finished_at = ?4
                 WHERE id = ?1 AND owner = ?5 AND status IN ('running', 'interrupted')",
                (
                    id,
                    status.as_str().to_string(),
                    result,
                    now(),
                    owner.to_string(),
                ),
            )
            .await?;
        Ok(changed > 0)
    }

    /// One job by number.
    pub async fn job_get(&self, id: i64) -> Result<Option<JobRow>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(&format!("SELECT {COLUMNS} FROM jobs WHERE id = ?1"), [id])
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row_to_job(&row)?)),
            None => Ok(None),
        }
    }

    /// Every job, newest first, with `result` left out (`None`): a list names jobs, and
    /// `job_get` fetches one answer.
    pub async fn job_list(&self) -> Result<Vec<JobRow>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT id, kind, label, status, NULL, owner, created_at, heartbeat_at,
                        finished_at, cancel_requested
                 FROM jobs ORDER BY id DESC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_job(&row)?);
        }
        Ok(out)
    }

    /// Refresh the heartbeat of the jobs `ids` that `owner` still runs, and return which of
    /// them another process asked to cancel. A row another process swept to `interrupted`
    /// while its owner was alive returns to `running`. One failing row does not stop the
    /// others: an error is logged and that job is retried at the next tick.
    pub async fn job_heartbeat(&self, owner: &str, ids: &[i64]) -> Result<Vec<i64>> {
        let conn = self.conn().await?;
        let ts = now();
        let mut cancel = Vec::new();
        for &id in ids {
            let beat = async {
                conn.execute(
                    "UPDATE jobs SET heartbeat_at = ?3, status = 'running', finished_at = NULL
                     WHERE id = ?1 AND owner = ?2 AND status IN ('running', 'interrupted')",
                    (id, owner.to_string(), ts),
                )
                .await?;
                let mut rows = conn
                    .query(
                        "SELECT 1 FROM jobs
                         WHERE id = ?1 AND owner = ?2 AND status = 'running'
                           AND cancel_requested = 1",
                        (id, owner.to_string()),
                    )
                    .await?;
                Ok::<bool, StoreError>(rows.next().await?.is_some())
            }
            .await;
            match beat {
                Ok(true) => cancel.push(id),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "kaibo::jobs", job = id, error = %e,
                    "job heartbeat failed for one job; retrying at the next tick"
                ),
            }
        }
        Ok(cancel)
    }

    /// Ask the owner of a `running` job to stop it. Returns whether the job was running.
    pub async fn job_request_cancel(&self, id: i64) -> Result<bool> {
        let conn = self.conn().await?;
        let changed = conn
            .execute(
                "UPDATE jobs SET cancel_requested = 1 WHERE id = ?1 AND status = 'running'",
                [id],
            )
            .await?;
        Ok(changed > 0)
    }

    /// Mark `running` rows with no recent heartbeat `interrupted`, and delete finished
    /// rows older than [`RETENTION_SECS`]. Returns how many rows were marked.
    pub async fn job_sweep_stale(&self) -> Result<u64> {
        self.job_sweep_at(now()).await
    }

    /// [`job_sweep_stale`](Self::job_sweep_stale) at an explicit time, for tests.
    pub async fn job_sweep_at(&self, at: i64) -> Result<u64> {
        let conn = self.conn().await?;
        let marked = conn
            .execute(
                "UPDATE jobs SET status = 'interrupted', finished_at = ?1
                 WHERE status = 'running' AND heartbeat_at < ?2",
                (at, at - STALE_AFTER_SECS),
            )
            .await?;
        conn.execute(
            "DELETE FROM jobs WHERE status != 'running' AND finished_at < ?1",
            [at - RETENTION_SECS],
        )
        .await?;
        Ok(marked)
    }
}
