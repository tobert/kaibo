//! Persistent session + batch-handle store on the pure-Rust `turso` crate.
//!
//! This is the durable twin of the in-memory [`crate::session::SessionStore`]: the same
//! capacity-LRU, no-TTL, touch-on-read-and-write semantics, but on a SQLite file so a
//! session (and the provider batch handles behind [`crate::batch`]) survive a process
//! restart and are shared across front doors — start a thread over MCP, continue it from
//! the CLI. The store holds only the lean `(question, answer)` turn log and the minimal
//! re-attachable batch record `{backend, provider_id, label}`; exploration reports and
//! tool transcripts stay ephemeral, exactly as the in-memory store keeps them.
//!
//! # Invariant posture — this is the amendment, not a loosening
//!
//! kaibo's headline invariant is "writes nothing, anywhere." This module is the honest,
//! scoped amendment: the *shell* (kaish) still has no write path, the sandbox's four
//! structural levers are untouched, and kaibo still never modifies the project. What is
//! new is a **handler-side** store at a **fixed, model-inaccessible state path** — the
//! content is model output, the path never is. Two guards keep that true by construction:
//!
//! - [`SessionStore::open`] **refuses any db path that resolves inside an allowed project
//!   tree** (canonicalizing the parent dir, since the file may not exist yet). A model can
//!   never coax a write onto disk through this file, because the file can never live
//!   where a model can reach — see [`SessionStore::open`] and its containment tests.
//! - The db is **reconstructible-or-disposable**: it is a convenience layer, never a
//!   source of truth. Corruption is handled by deleting the file and starting over, never
//!   by limping on — which keeps the "crash over corrupt data" principle intact.
//!
//! # The one open path, and why it is load-bearing
//!
//! [`build_database`] is the **only** code site that calls turso's [`Builder`]. That is a
//! deliberate, structural constraint, not a style choice. On 64-bit Unix it hardwires
//! `experimental_multiprocess_wal(true)` so the long-lived MCP server and short-lived CLI
//! invocations can share the file safely (empirically verified flawless across the exact
//! MCP+CLI shape). The lethal hazard the single-open-path defends against:
//!
//! > **Mixing a multiprocess-WAL open with a non-MP open of the same file silently loses
//! > acknowledged writes** — both opens succeed, operate on divergent WAL views, and
//! > `PRAGMA integrity_check` still reports `ok`. The upstream guard that is *documented*
//! > to reject a mixed open **does not fire in turso 0.7.0** (empirically verified
//! > 2026-07-17). This is precisely the silent-data-loss class kaibo's "crash over
//! > corrupt" principle exists to forbid.
//!
//! So the MP flag is hardwired here and this helper is the sole open site *by design*.
//! **Any future second open-site is a data-loss bug**, not a refactor — route every open
//! through [`build_database`].
//!
//! On Windows (and any non-64-bit-Unix target) MP mode is unavailable, so the store runs
//! default single-process mode: a concurrent second open fails loudly with a clear kaibo
//! error ([`StoreError::SingleProcessLocked`]) telling the operator to close the other
//! kaibo or disable persistence. Deliberate: the realistic Windows setup is kaibo + Claude
//! Code inside WSL, which is the Unix path anyway.
//!
//! # Concurrency
//!
//! The store holds the [`Database`] (a cheap `Clone`, `Send + Sync` `Arc` inside), never a
//! [`Connection`]: a single turso `Connection` forbids *concurrent* use, so under kaibo's
//! concurrent spawned consult tasks a shared connection would fail intermittently. Each
//! public operation mints a **fresh connection** ([`SessionStore::conn`]) with a
//! `busy_timeout`, and any multi-statement write runs in an explicit
//! `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on that one local connection (turso's
//! `Connection::transaction()` wants `&mut`, which the shared-`Database` shape can't give).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use turso::{Builder, Connection, Database, Value};

use crate::attach::Attachment;

/// Per-connection busy budget. A single turso `Connection` refuses *concurrent* use, so
/// the store mints a fresh connection per operation; this bounds how long one waits when
/// another connection holds a write lock before it gives up with `Busy`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Current on-disk schema version. Bump + add a migration arm when the shape changes;
/// migrations are forward-only and applied through [`SessionStore::migrate`].
///
/// - **v1** — `sessions`/`turns`/`batch_handles` (the durable session log + provider
///   batch-handle registry).
/// - **v2** — adds `local_jobs`/`local_job_items`: the queue + mailbox for the local
///   batch lane ([`SessionStore::enqueue_local`] and friends). A v1 db upgrades in place
///   (its session/batch data untouched); a fresh db is created at v2.
pub const SCHEMA_VERSION: i64 = 2;

/// How long a batch handle is kept before it's pruned. Provider batches expire around 30
/// days (Anthropic/Gemini both), so a handle older than that names a batch the provider has
/// already dropped — dead weight. Pruning at every [`SessionStore::put_batch`] and at
/// [`SessionStore::open`] bounds the table (and therefore `job_list`'s payload) via the data
/// itself, no query `LIMIT` needed. Sessions need no TTL — they're capacity-evicted.
const BATCH_HANDLE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// One completed turn — the lean `(question, answer)` pair, mirroring
/// [`crate::session::QaTurn`]. No exploration report, no tool transcript, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub question: String,
    pub answer: String,
}

/// A session as the list view sees it: identity plus its turn count and timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub turn_count: i64,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// A re-attachable provider batch handle. `backend`/`provider_id` rebuild the
/// `backend/provider_id` handle that [`crate::batch::poller`] re-addresses after a
/// restart — the fix for today's orphaned-batch problem (provider-side batches outlive
/// the process that submitted them, and kaibo held nothing on disk to find them again).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchHandle {
    pub backend: String,
    pub provider_id: String,
    pub label: Option<String>,
    pub created_at: i64,
}

/// Where a local batch job is in its lifecycle. Persisted as the lowercase string
/// ([`as_str`](Self::as_str)); the db is the queue, so this is the claim state a worker
/// flips (`Pending` → `Running` → `Done`/`Failed`) and the terminal a cancel sets
/// (`Cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalJobStatus {
    /// Enqueued, not yet claimed by a worker.
    Pending,
    /// Claimed by a worker and running its items.
    Running,
    /// Every item finished (each item may still carry a per-item error — the *job*
    /// completed).
    Done,
    /// A setup failure (the worker couldn't resolve the cast/arm) stopped the job before
    /// it could run its items; the items carry the reason.
    Failed,
    /// A cancel was requested; the worker stops between items (a running item finishes).
    Cancelled,
}

impl LocalJobStatus {
    /// The lowercase on-disk spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the on-disk spelling; an unknown value is a corrupt-store error (crash over
    /// silently mis-reading a status), never a silent default.
    fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StoreError::Corrupt(format!(
                "local job status {other:?} is not a known status"
            ))),
        }
    }
}

/// One prompt of a local batch job and its outcome. `result` is `None` until the worker
/// runs it, then `Some(Ok(text))` for the model's answer or `Some(Err(reason))` for a
/// per-item failure — the same per-item honesty the provider batch lane keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalJobItem {
    pub seq: i64,
    pub prompt: String,
    pub result: Option<std::result::Result<String, String>>,
    pub finished_at: Option<i64>,
}

/// A local batch job with its items — the whole record a worker or a `get` reads. The
/// attachments carry the CONTENT captured at submit (behind the store guard), so the
/// worker feeds the model the bytes as they were when submitted, not as they are now.
#[derive(Debug, Clone)]
pub struct LocalJob {
    pub id: i64,
    pub cast: String,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub status: LocalJobStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub attachments: Vec<Attachment>,
    pub items: Vec<LocalJobItem>,
}

/// A local batch job as the list view sees it — identity, status, lifecycle timestamps,
/// and item progress counts, without loading prompts/results/attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalJobSummary {
    pub id: i64,
    pub cast: String,
    pub status: LocalJobStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Total items in the job.
    pub total: i64,
    /// Items with a result (a model answer OR a per-item error).
    pub done: i64,
    /// Items whose result is a per-item error.
    pub failed: i64,
}

/// The outcome of a [`cancel_local`](SessionStore::cancel_local) request — reported
/// honestly so a caller learns whether a running item is still finishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelLocalOutcome {
    /// A pending job was cancelled before it ran.
    CancelledPending,
    /// A running job was marked cancelled; the worker stops after the in-flight item.
    CancellingRunning,
    /// The job was already cancelled — idempotent no-op.
    AlreadyCancelled,
    /// The job already finished (done/failed) — left alone.
    AlreadyFinished,
    /// No such job id.
    Unknown,
}

/// The store's error surface. A persistence-library boundary wants a typed error callers
/// can match on (unlike kaibo's anyhow-everywhere interior), and turso's own error is a
/// clean `thiserror` enum that maps naturally onto this one.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A turso engine error (busy, constraint, corrupt, I/O, …), surfaced verbatim.
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
    /// The requested db path resolves inside an allowed project tree — refused so the
    /// state db can never be pointed where a model can reach it (the invariant amendment).
    #[error("state db path must live outside every allowed project tree, but {0} is inside one")]
    PathInAllowedTree(String),
    /// The db path has no file-name component (a bare directory, a trailing `/`, or `..`).
    /// The state db must be a *file* path; a directory-only path has no db file to contain
    /// and would let the containment compare inspect only the parent — refused loudly.
    #[error("state db path {0} must name a file, not a bare directory")]
    InvalidStatePath(String),
    /// On a single-process platform (Windows / non-64-bit-Unix), another kaibo already
    /// holds the state db open. There is no concurrent-open mode there, so this is a
    /// clear refusal rather than silent divergence.
    #[error(
        "the kaibo state db {0} is already open by another process. On this platform the \
         state db is single-process: close the other kaibo (MCP server or CLI), or disable \
         persistence."
    )]
    SingleProcessLocked(String),
    /// The state dir lives on a network filesystem (NFS/CIFS/SMB), where multiprocess-WAL
    /// mode can silently lose acknowledged writes. Refused at open rather than risking it.
    #[error(
        "the kaibo state dir {0} is on a network filesystem (NFS/CIFS); turso's \
         multiprocess-WAL mode is unsupported there and can silently lose writes. Point \
         the state dir at a local disk, or disable persistence."
    )]
    NetworkFilesystem(String),
    /// Creating the state directory failed (permissions, a file where a dir should be, …).
    /// The one blessed write site (see [`create_state_dir`]) reports its failure loudly
    /// rather than opening onto a path that can't hold the db.
    #[error("state dir: {0}")]
    StateDir(String),
    /// The store observed an impossible internal state (an aggregate query returning no
    /// row, etc.) — surfaced loudly rather than papered over.
    #[error("corrupt store: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A capacity-bounded, LRU-evicting persistent store of consult sessions plus a registry
/// of provider batch handles. `Clone`-cheap: [`Database`] is a `Send + Sync` `Arc`
/// internally, so every per-request handler clone shares one underlying db.
#[derive(Clone)]
pub struct SessionStore {
    db: Database,
    capacity: usize,
}

impl SessionStore {
    /// Open (creating the db file, and the state dir if absent) the state db at `path`,
    /// running schema init/migration.
    ///
    /// The parent state dir is created if missing — through [`create_state_dir`], the one
    /// blessed filesystem-mutating site in kaibo production code (the read-only invariant
    /// amendment; see that function and the carve-out in `tests/no_write_path.rs`). Creation
    /// happens *after* the containment check below, so a path aimed inside a project is
    /// refused with zero side effect — kaibo never creates a directory inside a project.
    /// Every other write kaibo makes still goes only through turso.
    ///
    /// `allowed_trees` are the project roots the read-only sandbox may reach; the state db
    /// must live *outside* all of them. A path that resolves inside any allowed tree is
    /// refused with [`StoreError::PathInAllowedTree`] — the containment guard that keeps a
    /// model-inaccessible write path model-inaccessible. The db file need not exist yet, so
    /// the guard canonicalizes the deepest *existing* ancestor of the parent directory and
    /// re-appends the rest, catching symlink and `..` traversal that a lexical compare
    /// would miss.
    ///
    /// Opening also validates that the state dir is on a local filesystem (network mounts
    /// are refused, see [`StoreError::NetworkFilesystem`]) and, on single-process
    /// platforms, maps a concurrent-open lock into [`StoreError::SingleProcessLocked`].
    pub async fn open(
        path: &Path,
        capacity: NonZeroUsize,
        allowed_trees: &[&Path],
    ) -> Result<Self> {
        // Absolutize up front so containment only ever compares absolute paths. A *relative*
        // db path whose parent doesn't exist would otherwise fall through the lexical
        // fallback still relative, and `starts_with` an absolute allowed tree trivially
        // misses it — the db then landing inside the project via cwd (Gemini review,
        // finding 1). Everything below (containment, dir creation, turso open) uses this one
        // absolute path.
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| {
                    StoreError::InvalidStatePath(format!(
                        "{}: cannot resolve current dir to absolutize a relative state db path: {e}",
                        path.display()
                    ))
                })?
                .join(path)
        };
        let path = path.as_path();

        // The state db must be a file, not a bare directory: a filename-less path (a
        // trailing `/`, `.`, `..`, or `/`) would let the containment compare below inspect
        // only the *parent* while the db itself lands AT the directory — so refuse it loudly
        // up front. `file_name()` is None for exactly those cases.
        let file_name = path
            .file_name()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| StoreError::InvalidStatePath(path.display().to_string()))?;

        // Containment — before we touch the disk at all, so a path aimed inside a project is
        // refused with zero side effect. Compare the resolved parent *plus the file name*
        // (the full db path), not just the parent: a path that equals an allowed tree has a
        // parent *above* the tree yet the db would land inside it, and only the full-path
        // compare catches that.
        let resolved_full = resolve_existing_parent(path).join(file_name);
        for tree in allowed_trees {
            let tree_resolved = tree.canonicalize().unwrap_or_else(|_| normalize(tree));
            if resolved_full.starts_with(&tree_resolved) {
                return Err(StoreError::PathInAllowedTree(path.display().to_string()));
            }
        }

        // Containment passed — now the state dir may be created and probed. Creating it is
        // the one blessed write (through `create_state_dir`); it can't be a data-loss event,
        // and it lets turso create the db file and the `statfs` probe below see a real dir.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_state_dir(parent)?;
            }
        }

        // Refuse a network-mounted state dir (MP-WAL is unsupported there).
        validate_local_filesystem(path.parent().unwrap_or(path))?;

        let db = build_database(path).await?;
        let store = Self {
            db,
            capacity: capacity.get(),
        };
        store.migrate().await?;
        // Sweep batch handles the provider has already expired (TTL). One prune at open
        // clears anything that aged out while kaibo was down, so `job_list` never serializes
        // long-dead handles even on a store that hasn't seen a `put_batch` in a while.
        {
            let conn = store.conn().await?;
            prune_stale_batch_handles(&conn, now()).await?;
        }
        Ok(store)
    }

    /// A fresh connection for one operation, with the busy budget applied. Cheap; the
    /// per-op discipline is what keeps concurrent callers off a single shared connection.
    async fn conn(&self) -> Result<Connection> {
        let conn = self.db.connect()?;
        let _ = conn.busy_timeout(BUSY_TIMEOUT);
        Ok(conn)
    }

    /// Read the `user_version` pragma and apply any pending migrations, forward-only. Each
    /// arm takes the file from N-1 to N; `user_version` persists in the db header across
    /// reopens, so a populated file skips straight through without re-applying schema.
    async fn migrate(&self) -> Result<()> {
        let conn = self.conn().await?;
        // WAL gives durability with concurrent readers; the pragma's return isn't load-bearing.
        let _ = conn.pragma_update("journal_mode", "WAL").await;

        let mut version = user_version(&conn).await?;
        while version < SCHEMA_VERSION {
            match version {
                0 => Self::apply_v1(&conn).await?,
                1 => Self::apply_v2(&conn).await?,
                other => {
                    return Err(StoreError::Corrupt(format!(
                        "no migration from schema version {other} (this binary knows up to \
                         {SCHEMA_VERSION}) — the state db is newer than the binary"
                    )))
                }
            }
            version += 1;
            conn.pragma_update("user_version", version).await?;
        }
        Ok(())
    }

    /// The current on-disk schema version (`PRAGMA user_version`) — for diagnostics and the
    /// migration tests (a fresh db reads [`SCHEMA_VERSION`]; a migrated v1 db reads it too,
    /// after [`migrate`](Self::migrate) has run at open).
    pub async fn schema_version(&self) -> Result<i64> {
        let conn = self.conn().await?;
        user_version(&conn).await
    }

    /// The v1 schema (from empty). `STRICT` tables throughout (turso 0.7 enables strict
    /// unconditionally); `ON CONFLICT` upserts for the session/batch rows.
    async fn apply_v1(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id           TEXT    PRIMARY KEY,
                created_at   INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL,
                touch_seq    INTEGER NOT NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS turns (
                session_id TEXT    NOT NULL,
                seq        INTEGER NOT NULL,
                question   TEXT    NOT NULL,
                answer     TEXT    NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq)
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_sessions_touch ON sessions(touch_seq);

            CREATE TABLE IF NOT EXISTS batch_handles (
                backend     TEXT    NOT NULL,
                provider_id TEXT    NOT NULL,
                label       TEXT,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (backend, provider_id)
            ) STRICT;
            "#,
        )
        .await?;
        Ok(())
    }

    /// The v2 schema (v1 → v2). Adds the **local batch** queue + mailbox: `local_jobs` (one
    /// row per submitted batch — its cast, optional model/backend override, captured
    /// attachment content, status, and lifecycle timestamps) and `local_job_items` (one row
    /// per prompt, with its per-item result text OR error filled in as the worker completes
    /// it). Attachment CONTENT is captured at submit into `local_jobs.attachments` (a JSON
    /// array) because the worker may run hours later, after the files have changed — the same
    /// inline-at-submit discipline the provider batch lane uses. Additive: the v1 tables are
    /// untouched, so a populated v1 db keeps every session and batch handle.
    async fn apply_v2(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS local_jobs (
                id          INTEGER PRIMARY KEY,
                cast_name   TEXT    NOT NULL,
                model       TEXT,
                backend     TEXT,
                attachments TEXT    NOT NULL,
                status      TEXT    NOT NULL,
                created_at  INTEGER NOT NULL,
                started_at  INTEGER,
                finished_at INTEGER
            ) STRICT;

            CREATE TABLE IF NOT EXISTS local_job_items (
                job_id       INTEGER NOT NULL,
                seq          INTEGER NOT NULL,
                prompt       TEXT    NOT NULL,
                result_text  TEXT,
                result_error TEXT,
                finished_at  INTEGER,
                PRIMARY KEY (job_id, seq)
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_local_jobs_status ON local_jobs(status);
            "#,
        )
        .await?;
        Ok(())
    }

    /// Append a completed turn to `id`, creating the session if new. Promotes `id` to
    /// most-recently-used; creating a session past the cap evicts the LRU one. The whole
    /// write — touch-seq allocation included — is one `BEGIN IMMEDIATE` transaction, so a
    /// concurrent writer sees a consistent snapshot and the touch key stays strictly
    /// increasing (see [`next_touch`]).
    pub async fn record_turn(&self, id: &str, question: &str, answer: &str) -> Result<()> {
        let conn = self.conn().await?;
        let ts = now();

        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = record_turn_inner(&conn, self.capacity, id, question, answer, ts).await;
        match result {
            Ok(()) => {
                conn.execute("COMMIT", ()).await?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(e)
            }
        }
    }

    /// Snapshot `id`'s prior turns oldest-first — empty if unknown. Touching promotes it to
    /// most-recently-used, so an actively-replayed thread can't be evicted out from under a
    /// client still asking on it (mirrors the in-memory store's `history`).
    ///
    /// The promote + read run in one `BEGIN IMMEDIATE` transaction, so a concurrent
    /// capacity-pushing `record_turn` cannot evict the session *between* our promotion and
    /// our read and hand us an empty history for a thread we just protected. An unknown id
    /// is a pure read: the promote `UPDATE` matches no row, allocates no touch key, and
    /// leaves the store byte-for-byte unchanged (mirroring the in-memory arm's zero side
    /// effects on a miss).
    pub async fn replay(&self, id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn().await?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = replay_inner(&conn, id).await;
        match result {
            Ok(turns) => {
                conn.execute("COMMIT", ()).await?;
                Ok(turns)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(e)
            }
        }
    }

    /// Live session count — for tests and diagnostics.
    pub async fn session_count(&self) -> Result<usize> {
        let conn = self.conn().await?;
        let mut rows = conn.query("SELECT COUNT(*) FROM sessions", ()).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| StoreError::Corrupt("COUNT(*) returned no row".into()))?;
        Ok(row.get::<i64>(0)? as usize)
    }

    /// The current maximum `touch_seq` (0 when empty) — the next allocation is this + 1.
    /// For tests and diagnostics: asserting that a no-op operation (e.g. replaying an
    /// unknown session) allocated nothing and left the LRU clock where it was.
    pub async fn max_touch_seq(&self) -> Result<i64> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query("SELECT COALESCE(MAX(touch_seq), 0) FROM sessions", ())
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| StoreError::Corrupt("MAX(touch_seq) returned no row".into()))?;
        Ok(row.get::<i64>(0)?)
    }

    /// All live sessions, most-recently-used first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.conn().await?;
        let mut out = Vec::new();
        let mut rows = conn
            .query(
                "SELECT s.id,
                        (SELECT COUNT(*) FROM turns t WHERE t.session_id = s.id),
                        s.created_at, s.last_used_at
                 FROM sessions s
                 ORDER BY s.touch_seq DESC",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            out.push(SessionInfo {
                id: row.get::<String>(0)?,
                turn_count: row.get::<i64>(1)?,
                created_at: row.get::<i64>(2)?,
                last_used_at: row.get::<i64>(3)?,
            });
        }
        Ok(out)
    }

    // --- Batch handles -----------------------------------------------------

    /// Record (or refresh) a provider batch handle so a restart can recover it. Upserts on
    /// the `(backend, provider_id)` key — a repeat submit updates the label rather than
    /// duplicating the row — and prunes any handles older than the TTL on the way, so the
    /// table (and `job_list`'s payload) stays bounded by the data itself.
    pub async fn put_batch(
        &self,
        backend: &str,
        provider_id: &str,
        label: Option<&str>,
    ) -> Result<()> {
        self.put_batch_at(backend, provider_id, label, now()).await
    }

    /// [`put_batch`](Self::put_batch) with an explicit `created_at` (epoch seconds) — the
    /// timestamp-injecting form, for tests exercising TTL pruning and for any future
    /// backfill. Prunes stale handles like `put_batch` does.
    pub async fn put_batch_at(
        &self,
        backend: &str,
        provider_id: &str,
        label: Option<&str>,
        created_at: i64,
    ) -> Result<()> {
        let conn = self.conn().await?;
        let label_val = match label {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        conn.execute(
            "INSERT INTO batch_handles (backend, provider_id, label, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(backend, provider_id) DO UPDATE SET label = ?3",
            (
                backend.to_string(),
                provider_id.to_string(),
                label_val,
                created_at,
            ),
        )
        .await?;
        prune_stale_batch_handles(&conn, now()).await?;
        Ok(())
    }

    /// Fetch one batch handle by its `(backend, provider_id)` key.
    pub async fn get_batch(&self, backend: &str, provider_id: &str) -> Result<Option<BatchHandle>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT backend, provider_id, label, created_at FROM batch_handles
                 WHERE backend = ?1 AND provider_id = ?2",
                (backend.to_string(), provider_id.to_string()),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row_to_handle(&row)?)),
            None => Ok(None),
        }
    }

    /// All known batch handles, newest first — the orphan-recovery view.
    pub async fn list_batches(&self) -> Result<Vec<BatchHandle>> {
        let conn = self.conn().await?;
        let mut out = Vec::new();
        let mut rows = conn
            .query(
                "SELECT backend, provider_id, label, created_at FROM batch_handles
                 ORDER BY created_at DESC",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            out.push(row_to_handle(&row)?);
        }
        Ok(out)
    }

    // --- Local batch jobs (schema v2) --------------------------------------

    /// Enqueue a local batch job: one `local_jobs` row (status `pending`) plus one
    /// `local_job_items` row per prompt. `attachments` are captured **by content** here
    /// (serialized into the row) so the worker feeds the model the bytes as they were at
    /// submit, even if it runs hours later. The whole enqueue is one `BEGIN IMMEDIATE`
    /// transaction, and the job id is `MAX(id)+1` allocated under that write lock — so two
    /// processes submitting at once get distinct, monotone ids with no external id source.
    /// Returns the new job id; the caller mints the `local/<id>` handle from it.
    pub async fn enqueue_local(
        &self,
        cast: &str,
        model: Option<&str>,
        backend: Option<&str>,
        attachments: &[Attachment],
        prompts: &[String],
    ) -> Result<i64> {
        if prompts.is_empty() {
            return Err(StoreError::Corrupt(
                "a local batch job needs at least one prompt".into(),
            ));
        }
        let attach_json = attachments_to_json(attachments)?;
        let conn = self.conn().await?;
        let ts = now();
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result =
            enqueue_local_inner(&conn, cast, model, backend, &attach_json, prompts, ts).await;
        match result {
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

    /// Atomically claim the oldest pending job for this worker: inside one `BEGIN IMMEDIATE`
    /// transaction, select the lowest-id `pending` job and flip it to `running` (stamping
    /// `started_at`). `IMMEDIATE` takes the write lock up front, so a second worker (even in
    /// another process on the same file) blocks until this commits and then reads no pending
    /// row for that id — **exactly one worker ever claims a given job**. Returns the claimed
    /// id, or `None` when the queue holds no pending job.
    pub async fn claim_next_local(&self) -> Result<Option<i64>> {
        let conn = self.conn().await?;
        let ts = now();
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = claim_next_local_inner(&conn, ts).await;
        match result {
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

    /// The full job record (base fields + attachments + items in seq order), or `None` for
    /// an unknown id.
    pub async fn get_local(&self, id: i64) -> Result<Option<LocalJob>> {
        let conn = self.conn().await?;
        let base = {
            let mut rows = conn
                .query(
                    "SELECT cast_name, model, backend, attachments, status, created_at, \
                     started_at, finished_at FROM local_jobs WHERE id = ?1",
                    [id],
                )
                .await?;
            match rows.next().await? {
                Some(row) => Some((
                    row.get::<String>(0)?,
                    value_opt_text(row.get_value(1)?),
                    value_opt_text(row.get_value(2)?),
                    row.get::<String>(3)?,
                    LocalJobStatus::parse(&row.get::<String>(4)?)?,
                    row.get::<i64>(5)?,
                    value_opt_i64(row.get_value(6)?),
                    value_opt_i64(row.get_value(7)?),
                )),
                None => None,
            }
        };
        let Some((cast, model, backend, attach_json, status, created_at, started_at, finished_at)) =
            base
        else {
            return Ok(None);
        };
        let attachments = attachments_from_json(&attach_json)?;

        let mut items = Vec::new();
        let mut rows = conn
            .query(
                "SELECT seq, prompt, result_text, result_error, finished_at \
                 FROM local_job_items WHERE job_id = ?1 ORDER BY seq ASC",
                [id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            items.push(row_to_local_item(&row)?);
        }
        Ok(Some(LocalJob {
            id,
            cast,
            model,
            backend,
            status,
            created_at,
            started_at,
            finished_at,
            attachments,
            items,
        }))
    }

    /// All local jobs as lightweight summaries, newest first — the list view.
    pub async fn list_local(&self) -> Result<Vec<LocalJobSummary>> {
        let conn = self.conn().await?;
        let mut out = Vec::new();
        let mut rows = conn
            .query(
                "SELECT j.id, j.cast_name, j.status, j.created_at, j.started_at, j.finished_at,
                        (SELECT COUNT(*) FROM local_job_items i WHERE i.job_id = j.id),
                        (SELECT COUNT(*) FROM local_job_items i WHERE i.job_id = j.id
                             AND (i.result_text IS NOT NULL OR i.result_error IS NOT NULL)),
                        (SELECT COUNT(*) FROM local_job_items i WHERE i.job_id = j.id
                             AND i.result_error IS NOT NULL)
                 FROM local_jobs j
                 ORDER BY j.id DESC",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            out.push(LocalJobSummary {
                id: row.get::<i64>(0)?,
                cast: row.get::<String>(1)?,
                status: LocalJobStatus::parse(&row.get::<String>(2)?)?,
                created_at: row.get::<i64>(3)?,
                started_at: value_opt_i64(row.get_value(4)?),
                finished_at: value_opt_i64(row.get_value(5)?),
                total: row.get::<i64>(6)?,
                done: row.get::<i64>(7)?,
                failed: row.get::<i64>(8)?,
            });
        }
        Ok(out)
    }

    /// Record one item's outcome as it completes — `Ok(text)` for an answer, `Err(reason)`
    /// for a per-item failure — and stamp its `finished_at`. Written per item (a single
    /// autocommit `UPDATE`), so a worker crash loses at most the one in-flight item.
    pub async fn record_local_item(
        &self,
        id: i64,
        seq: i64,
        result: std::result::Result<String, String>,
    ) -> Result<()> {
        let conn = self.conn().await?;
        let (text, err) = match result {
            Ok(t) => (Value::Text(t), Value::Null),
            Err(e) => (Value::Null, Value::Text(e)),
        };
        conn.execute(
            "UPDATE local_job_items SET result_text = ?3, result_error = ?4, finished_at = ?5 \
             WHERE job_id = ?1 AND seq = ?2",
            (id, seq, text, err, now()),
        )
        .await?;
        Ok(())
    }

    /// Finalize a job the worker drained to a terminal `status` (`Done` or `Failed`). Guarded
    /// `WHERE status = 'running'` so a concurrent [`cancel_local`](Self::cancel_local) that
    /// already flipped the job to `cancelled` **wins** — the worker's finalize then no-ops
    /// rather than clobbering the operator's cancel. Idempotent for the same reason.
    pub async fn mark_local_finished(&self, id: i64, status: LocalJobStatus) -> Result<()> {
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE local_jobs SET status = ?2, finished_at = ?3 \
             WHERE id = ?1 AND status = 'running'",
            (id, status.as_str().to_string(), now()),
        )
        .await?;
        Ok(())
    }

    /// The job's current status, or `None` if unknown. The worker reads this **between
    /// items** so an operator's cancel (which flips a running job to `cancelled`) stops it
    /// without a signal — the honest "checks status between items" semantics.
    pub async fn local_status(&self, id: i64) -> Result<Option<LocalJobStatus>> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query("SELECT status FROM local_jobs WHERE id = ?1", [id])
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(LocalJobStatus::parse(&row.get::<String>(0)?)?)),
            None => Ok(None),
        }
    }

    /// Cancel a local job. A `pending` job is cancelled outright; a `running` job is marked
    /// `cancelled` and the worker stops after its in-flight item finishes (it checks
    /// [`local_status`](Self::local_status) between items). A finished/already-cancelled job
    /// is left alone. One `BEGIN IMMEDIATE` transaction so the read-then-write can't race a
    /// concurrent finalize.
    pub async fn cancel_local(&self, id: i64) -> Result<CancelLocalOutcome> {
        let conn = self.conn().await?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = cancel_local_inner(&conn, id).await;
        match result {
            Ok(o) => {
                conn.execute("COMMIT", ()).await?;
                Ok(o)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(e)
            }
        }
    }
}

/// Create the persistence state directory (and any missing parents).
///
/// This is the **one blessed filesystem-mutating site** in kaibo production code — the
/// sanctioned half of the read-only invariant amendment (see the "Read-only is the product"
/// invariant in AGENTS.md). `tests/no_write_path.rs` carves out exactly this call: a
/// `create_dir_all` in `store.rs` carrying the marker on its own line, and nowhere else.
/// Any other `std::fs` mutation anywhere in `src/` — including a second `create_dir_all`, or
/// one without the marker — still fails that guard. kaibo's every other write goes only
/// through turso.
///
/// Callers must run the containment check first (as [`SessionStore::open`] does), so this
/// never creates a directory inside a project tree.
pub fn create_state_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir) // state-dir-create: blessed by the read-only invariant amendment
        .map_err(|e| StoreError::StateDir(format!("creating state dir {}: {e}", dir.display())))
}

/// The ONE and ONLY place that calls turso's [`Builder`]. See the module doc-comment:
/// mixing a multiprocess-WAL open with a non-MP open of the same file silently loses
/// acknowledged writes (with `integrity_check` still `ok`, and the documented upstream
/// rejection guard NOT firing in turso 0.7.0, empirically verified 2026-07-17). That is
/// why the MP flag is hardwired on the platforms that support it, why this is the sole
/// open site, and why **any future second open-site is a data-loss bug** — route every
/// open through here.
///
/// On 64-bit Unix, `experimental_multiprocess_wal(true)` is hardwired so the long-lived
/// MCP server and short-lived CLI invocations share the file safely. On Windows and any
/// non-64-bit-Unix target MP mode is unavailable, so we open default (single-process) and
/// map turso's lock error into a clear [`StoreError::SingleProcessLocked`]. The lock-error
/// mapping compiles on every platform (only the flag line is `cfg`-gated), so its behavior
/// stays reviewable here rather than hiding in a Windows-only branch.
async fn build_database(path: &Path) -> Result<Database> {
    let builder = Builder::new_local(&path.to_string_lossy());
    #[cfg(all(unix, target_pointer_width = "64"))]
    let builder = builder.experimental_multiprocess_wal(true);

    match builder.build().await {
        Ok(db) => Ok(db),
        Err(e) if is_lock_error(&e) => {
            Err(StoreError::SingleProcessLocked(path.display().to_string()))
        }
        Err(e) => Err(StoreError::Turso(e)),
    }
}

/// Does this turso error mean the file is already locked by another process? turso 0.7.0
/// has no dedicated lock variant — the open failure surfaces as `Error::Error(msg)` whose
/// text names the lock ("Locking error: … File is locked by another process") — so we
/// match on the message. Positive, narrow match: only a genuine lock message maps to the
/// single-process refusal; anything else stays a raw turso error.
fn is_lock_error(e: &turso::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("locked") || msg.contains("locking")
}

async fn user_version(conn: &Connection) -> Result<i64> {
    let mut v = 0i64;
    conn.pragma_query("user_version", |row| {
        v = row.get::<i64>(0).unwrap_or(0);
        Ok(())
    })
    .await?;
    Ok(v)
}

/// The next touch-seq value: `MAX(touch_seq)+1` over `sessions` (1 when empty). A
/// clock-free LRU key. **Strictly increasing when called inside a `BEGIN IMMEDIATE`
/// transaction** (as both writers do): `IMMEDIATE` takes the write lock up front, so a
/// concurrent writer blocks until we commit and then reads a strictly larger max — no two
/// committed writes tie. (turso 0.7 rejects this as an inline `SET`/`VALUES` subquery, so it
/// is a separate statement; correctness rests on the enclosing transaction, not autocommit.)
async fn next_touch(conn: &Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(touch_seq), 0) + 1 FROM sessions", ())
        .await?;
    match rows.next().await? {
        Some(row) => Ok(row.get::<i64>(0)?),
        None => Ok(1),
    }
}

/// Does a session with this id exist? Used inside [`replay_inner`]'s transaction so an
/// unknown id promotes nothing (no touch allocated, no row touched) — a pure read.
async fn session_exists(conn: &Connection, id: &str) -> Result<bool> {
    let mut rows = conn
        .query("SELECT 1 FROM sessions WHERE id = ?1", [id.to_string()])
        .await?;
    Ok(rows.next().await?.is_some())
}

/// The promote + read of [`SessionStore::replay`], run inside its transaction. Promotes only
/// an *existing* session — an unknown id skips the promote entirely (no touch consumed, no
/// row changed), so a miss leaves the store byte-for-byte unchanged.
async fn replay_inner(conn: &Connection, id: &str) -> Result<Vec<Turn>> {
    if session_exists(conn, id).await? {
        let touch = next_touch(conn).await?;
        conn.execute(
            "UPDATE sessions SET last_used_at = ?2, touch_seq = ?3 WHERE id = ?1",
            (id.to_string(), now(), touch),
        )
        .await?;
    }

    let mut out = Vec::new();
    let mut rows = conn
        .query(
            "SELECT question, answer FROM turns WHERE session_id = ?1 ORDER BY seq ASC",
            [id.to_string()],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        out.push(Turn {
            question: row.get::<String>(0)?,
            answer: row.get::<String>(1)?,
        });
    }
    Ok(out)
}

async fn record_turn_inner(
    conn: &Connection,
    capacity: usize,
    id: &str,
    question: &str,
    answer: &str,
    ts: i64,
) -> Result<()> {
    // Allocate the touch key inside the caller's transaction (findings: strictly increasing
    // under IMMEDIATE serialization), then upsert the session row, promoting it to MRU.
    let touch = next_touch(conn).await?;
    conn.execute(
        "INSERT INTO sessions (id, created_at, last_used_at, touch_seq)
                 VALUES (?1, ?2, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET last_used_at = ?2, touch_seq = ?3",
        (id.to_string(), ts, touch),
    )
    .await?;

    // Next ordinal within the session.
    let seq = {
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM turns WHERE session_id = ?1",
                [id.to_string()],
            )
            .await?;
        rows.next()
            .await?
            .ok_or_else(|| StoreError::Corrupt("MAX(seq) returned no row".into()))?
            .get::<i64>(0)?
    };

    conn.execute(
        "INSERT INTO turns (session_id, seq, question, answer, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            id.to_string(),
            seq,
            question.to_string(),
            answer.to_string(),
            ts,
        ),
    )
    .await?;

    evict_over_capacity(conn, capacity).await?;
    Ok(())
}

/// Drop the least-recently-used sessions (and their turns) beyond the cap. Mirrors
/// "evicted by capacity, not time": pressure comes only from new sessions, and only the
/// LRU tail is shed. Turns are deleted explicitly rather than leaning on a young FK/trigger
/// cascade path.
async fn evict_over_capacity(conn: &Connection, capacity: usize) -> Result<()> {
    let mut victims = Vec::new();
    {
        let mut rows = conn
            .query(
                "SELECT id FROM sessions ORDER BY touch_seq DESC LIMIT -1 OFFSET ?1",
                [capacity as i64],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            victims.push(row.get::<String>(0)?);
        }
    }
    for id in victims {
        conn.execute("DELETE FROM turns WHERE session_id = ?1", [id.clone()])
            .await?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])
            .await?;
    }
    Ok(())
}

/// Delete batch handles older than [`BATCH_HANDLE_TTL_SECS`] relative to `now`. A single
/// autocommit `DELETE`; called from `put_batch_at` and at `open` so the table can't grow
/// without bound across weeks of use (the provider has already expired anything this drops).
async fn prune_stale_batch_handles(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM batch_handles WHERE created_at < ?1",
        [now - BATCH_HANDLE_TTL_SECS],
    )
    .await?;
    Ok(())
}

/// Text of a nullable turso column (`Some` only for a genuine `Text`).
fn value_opt_text(v: Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s),
        _ => None,
    }
}

/// Integer of a nullable turso column (`Some` only for a genuine `Integer`).
fn value_opt_i64(v: Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(i),
        _ => None,
    }
}

/// A nullable text param: the value as `Text`, or SQL `NULL`.
fn opt_text_value(o: Option<&str>) -> Value {
    match o {
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    }
}

/// The next local-job id: `MAX(id)+1` (1 when empty). Strictly increasing when called
/// inside a `BEGIN IMMEDIATE` transaction (as [`enqueue_local`](SessionStore::enqueue_local)
/// does), so two concurrent submits get distinct ids.
async fn next_local_id(conn: &Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(id), 0) + 1 FROM local_jobs", ())
        .await?;
    match rows.next().await? {
        Some(row) => Ok(row.get::<i64>(0)?),
        None => Ok(1),
    }
}

async fn enqueue_local_inner(
    conn: &Connection,
    cast: &str,
    model: Option<&str>,
    backend: Option<&str>,
    attach_json: &str,
    prompts: &[String],
    ts: i64,
) -> Result<i64> {
    let id = next_local_id(conn).await?;
    conn.execute(
        "INSERT INTO local_jobs (id, cast_name, model, backend, attachments, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
        (
            id,
            cast.to_string(),
            opt_text_value(model),
            opt_text_value(backend),
            attach_json.to_string(),
            ts,
        ),
    )
    .await?;
    for (i, p) in prompts.iter().enumerate() {
        conn.execute(
            "INSERT INTO local_job_items (job_id, seq, prompt) VALUES (?1, ?2, ?3)",
            (id, i as i64, p.clone()),
        )
        .await?;
    }
    Ok(id)
}

async fn claim_next_local_inner(conn: &Connection, ts: i64) -> Result<Option<i64>> {
    // Read the id, then drop the statement before the UPDATE (one connection can't hold a
    // live result set open across another statement).
    let id = {
        let mut rows = conn
            .query(
                "SELECT id FROM local_jobs WHERE status = 'pending' ORDER BY id ASC LIMIT 1",
                (),
            )
            .await?;
        match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => return Ok(None),
        }
    };
    conn.execute(
        "UPDATE local_jobs SET status = 'running', started_at = ?2 WHERE id = ?1",
        (id, ts),
    )
    .await?;
    Ok(Some(id))
}

async fn cancel_local_inner(conn: &Connection, id: i64) -> Result<CancelLocalOutcome> {
    let status = {
        let mut rows = conn
            .query("SELECT status FROM local_jobs WHERE id = ?1", [id])
            .await?;
        match rows.next().await? {
            Some(row) => LocalJobStatus::parse(&row.get::<String>(0)?)?,
            None => return Ok(CancelLocalOutcome::Unknown),
        }
    };
    match status {
        LocalJobStatus::Pending => {
            conn.execute(
                "UPDATE local_jobs SET status = 'cancelled', finished_at = ?2 WHERE id = ?1",
                (id, now()),
            )
            .await?;
            Ok(CancelLocalOutcome::CancelledPending)
        }
        LocalJobStatus::Running => {
            conn.execute(
                "UPDATE local_jobs SET status = 'cancelled', finished_at = ?2 WHERE id = ?1",
                (id, now()),
            )
            .await?;
            Ok(CancelLocalOutcome::CancellingRunning)
        }
        LocalJobStatus::Cancelled => Ok(CancelLocalOutcome::AlreadyCancelled),
        LocalJobStatus::Done | LocalJobStatus::Failed => Ok(CancelLocalOutcome::AlreadyFinished),
    }
}

fn row_to_local_item(row: &turso::Row) -> Result<LocalJobItem> {
    let seq = row.get::<i64>(0)?;
    let prompt = row.get::<String>(1)?;
    let text = value_opt_text(row.get_value(2)?);
    let err = value_opt_text(row.get_value(3)?);
    let finished_at = value_opt_i64(row.get_value(4)?);
    // Text wins if (impossibly) both are set — an answer is more useful than an error, and
    // the worker only ever writes one of the two.
    let result = match (text, err) {
        (Some(t), _) => Some(Ok(t)),
        (None, Some(e)) => Some(Err(e)),
        (None, None) => None,
    };
    Ok(LocalJobItem {
        seq,
        prompt,
        result,
        finished_at,
    })
}

/// Serialize captured attachments to the JSON stored in `local_jobs.attachments`.
fn attachments_to_json(atts: &[Attachment]) -> Result<String> {
    let arr: Vec<serde_json::Value> = atts
        .iter()
        .map(|a| match a {
            Attachment::Text { path, body } => {
                serde_json::json!({ "kind": "text", "path": path, "body": body })
            }
            Attachment::Image {
                path,
                mime,
                data_b64,
            } => serde_json::json!({
                "kind": "image", "path": path, "mime": mime, "data_b64": data_b64
            }),
        })
        .collect();
    serde_json::to_string(&arr)
        .map_err(|e| StoreError::Corrupt(format!("serializing attachments: {e}")))
}

/// Rebuild attachments from the stored JSON. A missing field or an unknown kind/mime is a
/// corrupt-store error (crash over silently feeding the model wrong bytes), never a drop.
fn attachments_from_json(s: &str) -> Result<Vec<Attachment>> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(s)
        .map_err(|e| StoreError::Corrupt(format!("parsing stored attachments: {e}")))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "text" => out.push(Attachment::Text {
                path: json_str_field(&v, "path")?,
                body: json_str_field(&v, "body")?,
            }),
            "image" => {
                let mime_s = json_str_field(&v, "mime")?;
                let mime = crate::attach::intern_image_mime(&mime_s).ok_or_else(|| {
                    StoreError::Corrupt(format!(
                        "stored attachment has unknown image mime {mime_s:?}"
                    ))
                })?;
                out.push(Attachment::Image {
                    path: json_str_field(&v, "path")?,
                    mime,
                    data_b64: json_str_field(&v, "data_b64")?,
                });
            }
            other => {
                return Err(StoreError::Corrupt(format!(
                    "stored attachment has unknown kind {other:?}"
                )))
            }
        }
    }
    Ok(out)
}

/// Read a required string field out of a stored attachment object.
fn json_str_field(v: &serde_json::Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|f| f.as_str())
        .map(str::to_string)
        .ok_or_else(|| StoreError::Corrupt(format!("stored attachment missing string {key:?}")))
}

fn row_to_handle(row: &turso::Row) -> Result<BatchHandle> {
    let label = match row.get_value(2)? {
        Value::Null => None,
        Value::Text(s) => Some(s),
        other => Some(format!("{other:?}")),
    };
    Ok(BatchHandle {
        backend: row.get::<String>(0)?,
        provider_id: row.get::<String>(1)?,
        label,
        created_at: row.get::<i64>(3)?,
    })
}

/// Resolve the db file's parent to a canonical, absolute path for the containment compare.
/// The file — and even its immediate parent — may not exist yet, so we canonicalize the
/// deepest *existing* ancestor (resolving symlinks and `..` there) and re-append the
/// not-yet-created tail lexically. This gives the guard teeth a lexical compare lacks: a
/// symlink whose target is inside a project is caught, because the existing-ancestor
/// canonicalization follows it.
fn resolve_existing_parent(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // Absolutize a relative path against cwd first, so `starts_with` compares like-for-like.
    let parent = if parent.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        parent.to_path_buf()
    };
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor: &Path = &parent;
    loop {
        if let Ok(canon) = cursor.canonicalize() {
            let mut base = canon;
            for name in tail.iter().rev() {
                base.push(name);
            }
            return normalize(&base);
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(up)) => {
                tail.push(name.to_owned());
                cursor = up;
            }
            _ => break,
        }
    }
    // Nothing along the path exists (or it has no anchor to canonicalize) — fall back to a
    // lexical normalize of the absolutized parent.
    normalize(&parent)
}

/// Lexically clean a path (resolve `.` and `..` without touching the filesystem). Used to
/// re-append not-yet-created components after canonicalizing the existing ancestor, and as
/// the fallback when nothing along the path exists yet.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Refuse a state dir on a network filesystem, where turso's multiprocess-WAL mode is
/// unsupported and can silently lose acknowledged writes. The cheapest honest check: on
/// 64-bit Linux (where MP mode is active) `statfs` the dir and compare `f_type` against the
/// known network-fs magics. Elsewhere it is a documented no-op — the magics aren't portable,
/// and this is a best-effort guard, not a load-bearing one (the single-open-path discipline
/// is what makes MP safe).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn validate_local_filesystem(dir: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = match CString::new(dir.as_os_str().as_bytes()) {
        Ok(c) => c,
        // An interior NUL means we can't ask; don't block on an un-askable path.
        Err(_) => return Ok(()),
    };
    // SAFETY: `buf` is a zeroed, owned `statfs`; `c_path` is a valid NUL-terminated path.
    // `statfs` only writes into `buf` and reads the path; failure is reported via rc.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        // Couldn't determine the fs type — a stat failure is not proof of a network mount,
        // so don't refuse on it.
        return Ok(());
    }
    // Keep the low 32 bits so the compare is width-agnostic across arches.
    let magic = (buf.f_type as i64) & 0xFFFF_FFFF;
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const CIFS_MAGIC_NUMBER: i64 = 0xFF53_4D42;
    const SMB2_MAGIC_NUMBER: i64 = 0xFE53_4D42;
    if matches!(
        magic,
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | CIFS_MAGIC_NUMBER | SMB2_MAGIC_NUMBER
    ) {
        return Err(StoreError::NetworkFilesystem(dir.display().to_string()));
    }
    Ok(())
}

/// Non-Linux / non-64-bit: no portable `statfs` magic check, so this is a documented
/// no-op. The limitation is called out in the module docs; MP mode's safety rests on the
/// single-open-path discipline, not on this best-effort guard.
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn validate_local_filesystem(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    //! White-box tests for the `user_version` migration machinery — they reach the private
    //! `build_database`/`apply_v1`/`user_version` seams, which the external `tests/store.rs`
    //! integration suite can't. The point is to prove the migration ladder has teeth: a
    //! fresh db is created at the latest version, and a hand-built **v1** db upgrades in
    //! place to v2 without losing its v1 data. (Forget the `1 => apply_v2` arm and the v1
    //! upgrade below fails loudly.)

    use super::*;
    use tempfile::TempDir;

    fn cap() -> NonZeroUsize {
        NonZeroUsize::new(4).unwrap()
    }

    /// Build a db on disk that is genuinely at schema v1: v1 tables, a seeded session, and
    /// `user_version = 1` — the state a prior kaibo binary left behind.
    async fn make_v1_db(path: &Path) {
        let db = build_database(path).await.expect("build v1 db");
        let conn = db.connect().expect("connect");
        let _ = conn.pragma_update("journal_mode", "WAL").await;
        SessionStore::apply_v1(&conn).await.expect("apply v1");
        // Seed a session + turn so we can prove v1 data survives the migration.
        conn.execute(
            "INSERT INTO sessions (id, created_at, last_used_at, touch_seq) VALUES ('s', 1, 1, 1)",
            (),
        )
        .await
        .expect("seed session");
        conn.execute(
            "INSERT INTO turns (session_id, seq, question, answer, created_at) \
             VALUES ('s', 0, 'q1', 'a1', 1)",
            (),
        )
        .await
        .expect("seed turn");
        conn.pragma_update("user_version", 1i64)
            .await
            .expect("stamp v1");
        assert_eq!(user_version(&conn).await.unwrap(), 1, "db is really at v1");
    }

    #[tokio::test]
    async fn fresh_db_is_created_at_the_latest_version() {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::open(&dir.path().join("state.db"), cap(), &[])
            .await
            .expect("open fresh");
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 2, "this test pins the current version");
        // The v2 local-job table exists and is usable on a fresh db.
        let id = store
            .enqueue_local("deepseek", None, None, &[], &["hi".to_string()])
            .await
            .expect("enqueue on fresh v2 db");
        assert_eq!(id, 1);
    }

    #[tokio::test]
    async fn migrates_a_v1_db_in_place_to_v2_keeping_v1_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        make_v1_db(&path).await;

        // Reopen through the real path: `open` runs `migrate`, which must carry v1 → v2.
        let store = SessionStore::open(&path, cap(), &[])
            .await
            .expect("reopen migrates in place");
        assert_eq!(
            store.schema_version().await.unwrap(),
            2,
            "the v1 db upgraded in place to v2"
        );
        // v1 data survived the migration untouched.
        assert_eq!(
            store.replay("s").await.unwrap(),
            vec![Turn {
                question: "q1".into(),
                answer: "a1".into()
            }],
            "the pre-migration session must survive"
        );
        // And the freshly-added v2 surface works on the migrated db.
        let id = store
            .enqueue_local("deepseek", None, None, &[], &["p".to_string()])
            .await
            .expect("enqueue on migrated db");
        let job = store.get_local(id).await.unwrap().expect("job exists");
        assert_eq!(job.status, LocalJobStatus::Pending);
        assert_eq!(job.items.len(), 1);
    }
}
