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

/// Per-connection busy budget. A single turso `Connection` refuses *concurrent* use, so
/// the store mints a fresh connection per operation; this bounds how long one waits when
/// another connection holds a write lock before it gives up with `Busy`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Current on-disk schema version. Bump + add a migration arm when the shape changes;
/// migrations are forward-only and applied through [`SessionStore::migrate`].
pub const SCHEMA_VERSION: i64 = 1;

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
    /// Open (creating the db file if absent) the state db at `path`, running schema
    /// init/migration.
    ///
    /// The **parent state dir must already exist** — the store performs no `std::fs`
    /// mutation of its own (that would breach the source-level read-only guard in
    /// `tests/no_write_path.rs`; the store's writes go only through turso, the deliberate,
    /// contained persistence capability). Creating the XDG state dir belongs to whoever
    /// owns that path — the config/path layer that resolves it (stage 2), not the store. A
    /// missing parent surfaces as a loud turso open error rather than a silent create.
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
        // Containment first — before we touch the disk at all, so a path aimed inside a
        // project is refused with zero side effect.
        let resolved_parent = resolve_existing_parent(path);
        for tree in allowed_trees {
            let tree_resolved = tree.canonicalize().unwrap_or_else(|_| normalize(tree));
            if resolved_parent.starts_with(&tree_resolved) {
                return Err(StoreError::PathInAllowedTree(path.display().to_string()));
            }
        }

        // Refuse a network-mounted state dir (MP-WAL is unsupported there). Best-effort: if
        // the parent doesn't exist yet, the probe declines to block and the missing-dir
        // failure surfaces from the turso open below.
        validate_local_filesystem(path.parent().unwrap_or(path))?;

        let db = build_database(path).await?;
        let store = Self {
            db,
            capacity: capacity.get(),
        };
        store.migrate().await?;
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
                status      TEXT,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (backend, provider_id)
            ) STRICT;
            "#,
        )
        .await?;
        Ok(())
    }

    /// Append a completed turn to `id`, creating the session if new. Promotes `id` to
    /// most-recently-used; creating a session past the cap evicts the LRU one. The whole
    /// write is one transaction on one connection.
    pub async fn record_turn(&self, id: &str, question: &str, answer: &str) -> Result<()> {
        let conn = self.conn().await?;
        let ts = now();
        let touch = next_touch(&conn).await?;

        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let result = record_turn_inner(&conn, self.capacity, id, question, answer, ts, touch).await;
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
    pub async fn replay(&self, id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn().await?;
        let ts = now();
        let touch = next_touch(&conn).await?;
        conn.execute(
            "UPDATE sessions SET last_used_at = ?2, touch_seq = ?3 WHERE id = ?1",
            (id.to_string(), ts, touch),
        )
        .await?;

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

    /// Record (or refresh) a provider batch handle so a restart can re-attach to it.
    /// Upserts on the `(backend, provider_id)` key — a repeat submit updates the label
    /// rather than duplicating the row.
    pub async fn put_batch(
        &self,
        backend: &str,
        provider_id: &str,
        label: Option<&str>,
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
                now(),
            ),
        )
        .await?;
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

/// Next strictly-increasing touch sequence — a clock-free LRU ordering key, so eviction is
/// deterministic even when several sessions share a wall-clock second (tests depend on it).
async fn next_touch(conn: &Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(touch_seq), 0) + 1 FROM sessions", ())
        .await?;
    match rows.next().await? {
        Some(row) => Ok(row.get::<i64>(0)?),
        None => Ok(1),
    }
}

async fn record_turn_inner(
    conn: &Connection,
    capacity: usize,
    id: &str,
    question: &str,
    answer: &str,
    ts: i64,
    touch: i64,
) -> Result<()> {
    // Upsert the session row, promoting it to MRU.
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
