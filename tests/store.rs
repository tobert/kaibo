//! Behavioral tests for the persistent [`kaibo::store::SessionStore`].
//!
//! Two jobs: prove parity with the in-memory `session.rs` semantics (capacity-LRU, no
//! TTL, touch-on-read-and-write promotion, clone-sharing, order preservation) on a durable
//! file, and prove the persistence-specific and invariant-amendment properties (survives
//! reopen, migration idempotence, batch put/get/list/upsert, the allowed-tree containment
//! guard with symlink teeth, concurrent writes, and Send futures).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use kaibo::store::{CredentialStore, SessionStore, StoreError, Turn};
use tempfile::TempDir;

fn cap(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// A fresh state db under a temp dir plus the dir (kept alive by the caller).
async fn open(capacity: usize) -> (SessionStore, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    // The allowed tree is some *other* project dir, never where the state db lives.
    let store = SessionStore::open(&path, cap(capacity), &[])
        .await
        .expect("open store");
    (store, dir)
}

fn turn(q: &str, a: &str) -> Turn {
    Turn {
        question: q.into(),
        answer: a.into(),
    }
}

#[tokio::test]
async fn unknown_session_has_empty_history() {
    let (store, _d) = open(4).await;
    assert!(store.replay("nope").await.unwrap().is_empty());
    assert_eq!(store.session_count().await.unwrap(), 0);
}

#[tokio::test]
async fn record_then_replay_round_trips_and_accumulates_in_order() {
    let (store, _d) = open(4).await;
    store.record_turn("s", "q1", "a1").await.unwrap();
    store.record_turn("s", "q2", "a2").await.unwrap();

    assert_eq!(
        store.replay("s").await.unwrap(),
        vec![turn("q1", "a1"), turn("q2", "a2")],
        "turns must accumulate oldest-first under one session"
    );
    assert_eq!(store.session_count().await.unwrap(), 1);
}

#[tokio::test]
async fn distinct_ids_are_distinct_sessions() {
    let (store, _d) = open(4).await;
    store.record_turn("a", "qa", "aa").await.unwrap();
    store.record_turn("b", "qb", "ab").await.unwrap();
    assert_eq!(store.replay("a").await.unwrap(), vec![turn("qa", "aa")]);
    assert_eq!(store.replay("b").await.unwrap(), vec![turn("qb", "ab")]);
    assert_eq!(store.session_count().await.unwrap(), 2);
}

#[tokio::test]
async fn over_capacity_evicts_the_least_recently_used() {
    let (store, _d) = open(2).await;
    store.record_turn("s1", "q1", "a1").await.unwrap();
    store.record_turn("s2", "q2", "a2").await.unwrap();
    // s3 pushes past cap 2 -> s1 (LRU) is evicted.
    store.record_turn("s3", "q3", "a3").await.unwrap();

    assert!(
        store.replay("s1").await.unwrap().is_empty(),
        "s1 should have been evicted"
    );
    assert_eq!(store.replay("s2").await.unwrap(), vec![turn("q2", "a2")]);
    assert_eq!(store.replay("s3").await.unwrap(), vec![turn("q3", "a3")]);
    assert_eq!(store.session_count().await.unwrap(), 2);
}

#[tokio::test]
async fn touching_a_session_protects_it_from_eviction() {
    let (store, _d) = open(2).await;
    store.record_turn("s1", "q1", "a1").await.unwrap();
    store.record_turn("s2", "q2", "a2").await.unwrap();
    // Replaying s1 promotes it to MRU, so s2 becomes the LRU.
    let _ = store.replay("s1").await.unwrap();
    store.record_turn("s3", "q3", "a3").await.unwrap();

    assert_eq!(
        store.replay("s1").await.unwrap(),
        vec![turn("q1", "a1")],
        "s1 was just touched, so it must survive"
    );
    assert!(
        store.replay("s2").await.unwrap().is_empty(),
        "s2 became the LRU and should be evicted"
    );
}

#[tokio::test]
async fn clones_share_one_store() {
    let (store, _d) = open(4).await;
    let clone = store.clone();
    store.record_turn("s", "q", "a").await.unwrap();
    assert_eq!(clone.replay("s").await.unwrap(), vec![turn("q", "a")]);
}

/// The headline requirement: survive a process restart. Reopen the same file and the
/// sessions + turns (and batch handles) are still there, oldest-first.
#[tokio::test]
async fn survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    {
        let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
        store.record_turn("keep", "q1", "a1").await.unwrap();
        store.record_turn("keep", "q2", "a2").await.unwrap();
        store
            .put_batch("anthropic", "msgbatch_123", Some("nightly"), Some(10))
            .await
            .unwrap();
    }
    // New connection, same file — as a short-lived CLI invocation would see it.
    let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    assert_eq!(
        store.replay("keep").await.unwrap(),
        vec![turn("q1", "a1"), turn("q2", "a2")],
        "turns must survive a reopen"
    );
    let handle = store
        .get_batch("anthropic", "msgbatch_123")
        .await
        .unwrap()
        .expect("batch handle survives reopen");
    assert_eq!(handle.label.as_deref(), Some("nightly"));
    assert_eq!(
        handle.submitted_count,
        Some(10),
        "the submitted count must survive a reopen too"
    );
}

#[tokio::test]
async fn list_sessions_is_mru_first() {
    let (store, _d) = open(8).await;
    store.record_turn("a", "q", "a").await.unwrap();
    store.record_turn("b", "q", "a").await.unwrap();
    store.record_turn("c", "q", "a").await.unwrap();
    // Touch "a" so it jumps to the front.
    let _ = store.replay("a").await.unwrap();

    let ids: Vec<String> = store
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec!["a", "c", "b"], "MRU-first ordering");
}

#[tokio::test]
async fn batch_put_get_list_and_upsert() {
    let (store, _d) = open(8).await;
    store
        .put_batch("gemini", "batches/abc", None, Some(1))
        .await
        .unwrap();
    store
        .put_batch("anthropic", "msgbatch_1", Some("hard-q"), Some(3))
        .await
        .unwrap();
    // Upsert: same key, new label and count — one row, updated.
    store
        .put_batch("anthropic", "msgbatch_1", Some("hard-q-v2"), Some(5))
        .await
        .unwrap();

    let g = store
        .get_batch("gemini", "batches/abc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g.label, None);
    assert_eq!(g.submitted_count, Some(1));
    let a = store
        .get_batch("anthropic", "msgbatch_1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        a.label.as_deref(),
        Some("hard-q-v2"),
        "upsert updated label"
    );
    assert_eq!(
        a.submitted_count,
        Some(5),
        "upsert updated the submitted count too"
    );

    let all = store.list_batches().await.unwrap();
    assert_eq!(all.len(), 2, "upsert did not create a duplicate row");
    assert!(store.get_batch("nope", "nope").await.unwrap().is_none());
}

/// Finding 2 (Gemini review): batch handles are TTL-pruned so the table — and `job_list`'s
/// payload — can't grow without bound. A handle older than ~30 days (past provider expiry)
/// is swept; a fresh one is kept. Pruning runs on every `put_batch`.
#[tokio::test]
async fn stale_batch_handles_are_pruned_by_ttl() {
    let (store, _d) = open(8).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let day = 24 * 60 * 60;

    // A handle 40 days old (past the 30-day TTL) and a 1-day-old one.
    store
        .put_batch_at("anthropic", "old", Some("stale"), Some(1), now - 40 * day)
        .await
        .unwrap();
    store
        .put_batch_at("anthropic", "new", Some("fresh"), Some(1), now - day)
        .await
        .unwrap();

    assert!(
        store.get_batch("anthropic", "old").await.unwrap().is_none(),
        "a handle past the TTL must be pruned"
    );
    assert!(
        store.get_batch("anthropic", "new").await.unwrap().is_some(),
        "a fresh handle must be kept"
    );
    assert_eq!(
        store.list_batches().await.unwrap().len(),
        1,
        "job_list sees only live handles"
    );
}

#[tokio::test]
async fn reopen_does_not_re_run_migration() {
    // Reopening a populated db must not wipe or double-apply schema.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    {
        let s = SessionStore::open(&path, cap(4), &[]).await.unwrap();
        s.record_turn("x", "q", "a").await.unwrap();
    }
    let s = SessionStore::open(&path, cap(4), &[]).await.unwrap();
    assert_eq!(s.replay("x").await.unwrap(), vec![turn("q", "a")]);
}

/// The sandbox-separation invariant: the store refuses a path inside an allowed
/// (model-reachable) project tree, so a model can never coax a write onto disk through
/// this file. The invariant-amendment test.
#[tokio::test]
async fn refuses_path_inside_allowed_tree() {
    let project = TempDir::new().unwrap();
    let inside = project.path().join("sub/state.db");
    match SessionStore::open(&inside, cap(4), &[project.path()]).await {
        Err(StoreError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("must refuse a path inside an allowed tree"),
    }
    // The guard must reject *before* any side effect — the not-yet-existing subdir must
    // not have been created on the way to refusing.
    assert!(
        !project.path().join("sub").exists(),
        "containment refusal must not create the state dir it refused"
    );
}

/// Teeth beyond a lexical compare: a state path that reaches into a project *through a
/// symlink* must still be refused. A lexical/normalize-only guard would miss this because
/// the symlink component doesn't lexically start with the project path — only canonicalizing
/// the existing ancestor (which follows the symlink) catches it. This is the test that
/// would fail against a naive lexical guard.
#[cfg(unix)]
#[tokio::test]
async fn refuses_path_reaching_into_tree_via_symlink() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join("real")).unwrap();

    let elsewhere = TempDir::new().unwrap();
    // `link` lives outside the project but points *into* it.
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(project.path().join("real"), &link).unwrap();

    // A db path through the symlink resolves inside the project — must be refused.
    let sneaky = link.join("state.db");
    match SessionStore::open(&sneaky, cap(4), &[project.path()]).await {
        Err(StoreError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("must refuse a symlinked path that resolves inside an allowed tree"),
    }
}

#[tokio::test]
async fn allows_path_outside_allowed_trees() {
    let project = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let path = state_dir.path().join("state.db");
    // state dir is a sibling temp dir, not under the project tree — allowed.
    let store = SessionStore::open(&path, cap(4), &[project.path()])
        .await
        .expect("path outside allowed trees is fine");
    store.record_turn("s", "q", "a").await.unwrap();
    assert_eq!(store.replay("s").await.unwrap(), vec![turn("q", "a")]);
}

/// The containment guard must compare the *full* db path, not just its parent. A path that
/// equals an allowed tree has a parent *above* the tree (which passes a parent-only check),
/// yet the db would land AT the tree root — so it must be refused. And a filename-less path
/// (a bare directory / `..`) has no db file to contain and is refused loudly up front.
#[tokio::test]
async fn refuses_bare_directory_and_path_equal_to_an_allowed_tree() {
    let project = TempDir::new().unwrap();

    // A db path that *is* the allowed tree: parent is the tree's parent (outside), but the
    // full path resolves onto the tree — the full-path compare must catch it.
    match SessionStore::open(project.path(), cap(4), &[project.path()]).await {
        Err(StoreError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error for path == allowed tree: {other:?}"),
        Ok(_) => panic!("a db path equal to an allowed tree must be refused"),
    }

    // A filename-less path (ends in `..`, so `file_name()` is None) is an invalid state path.
    let bare = project.path().join("sub").join("..");
    match SessionStore::open(&bare, cap(4), &[]).await {
        Err(StoreError::InvalidStatePath(_)) => {}
        Err(other) => panic!("wrong error for a bare-directory path: {other:?}"),
        Ok(_) => panic!("a bare-directory path must be refused as invalid"),
    }
}

/// Serializes the one test that mutates the process-global cwd, and restores it on drop.
/// (Every other store test uses absolute paths and never reads cwd, so this is belt-and-
/// suspenders against a future cwd-dependent test.)
struct CwdGuard {
    prev: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl CwdGuard {
    fn set(to: &Path) -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(to).unwrap();
        Self { prev, _lock: lock }
    }
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
    }
}

/// Finding 1 (Gemini review): a **relative** db path whose parent doesn't exist must not
/// slip the containment guard. Before the fix, `resolve_existing_parent` fell through to a
/// lexical fallback that stayed *relative*, so `starts_with` against the absolute allowed
/// tree trivially returned false — and the db landed inside the project via cwd. `open` now
/// absolutizes against cwd up front, so a relative path resolving into an allowed tree is
/// refused. (cwd == the project here, the realistic `kaibo --state-db rel/state.db` launched
/// from inside a repo.)
#[tokio::test]
async fn refuses_relative_path_that_absolutizes_into_an_allowed_tree() {
    let project = TempDir::new().unwrap();
    let canon = project.path().canonicalize().unwrap();
    let _cwd = CwdGuard::set(&canon);

    match SessionStore::open(Path::new("nonexistent_dir/state.db"), cap(4), &[&canon]).await {
        Err(StoreError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error for a relative in-cwd path: {other:?}"),
        Ok(_) => panic!("a relative path resolving inside the cwd allowed tree must be refused"),
    }
    // Nothing was created on the way to refusing (the guard runs before any disk write).
    assert!(
        !canon.join("nonexistent_dir").exists(),
        "containment refusal must not create the relative parent it refused"
    );
}

/// Finding 3: replaying an unknown session is a pure read — it promotes nothing and leaves
/// the LRU clock (`MAX(touch_seq)`) exactly where it was, mirroring the in-memory arm's
/// zero side effects on a miss. Would fail if the promote `UPDATE` (and its touch
/// allocation) ran unconditionally.
#[tokio::test]
async fn replay_of_unknown_session_has_no_side_effect() {
    let (store, _d) = open(8).await;
    store.record_turn("a", "q", "a").await.unwrap();
    store.record_turn("b", "q", "a").await.unwrap();
    let before = store.max_touch_seq().await.unwrap();
    assert!(
        before > 0,
        "recording two sessions advanced the touch clock"
    );

    for _ in 0..5 {
        assert!(store.replay("nonexistent").await.unwrap().is_empty());
    }

    assert_eq!(
        store.max_touch_seq().await.unwrap(),
        before,
        "replaying an unknown session must not advance the touch clock"
    );
    assert_eq!(
        store.session_count().await.unwrap(),
        2,
        "replaying an unknown session must not create a row"
    );
}

/// Finding 1: `replay` promotes and reads in one `BEGIN IMMEDIATE` transaction, so a
/// concurrent capacity-pushing `record_turn` cannot evict the session *between* the
/// promotion and the read and hand back an empty history for a thread it just protected.
/// The atomicity itself is **structural** — the single `BEGIN IMMEDIATE`/read/`COMMIT` in
/// `replay` (the transaction serializes writers, so the interleaving simply can't occur, and
/// can't be forced from outside). This test guards the surrounding contract under real
/// contention: many tasks replaying and recording (with eviction churn) on one file must all
/// succeed — no `Busy` storm, deadlock, or corruption — and the cap stays honored. A replayed
/// history, when non-empty, is always a valid oldest-first prefix (never a torn read).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_replay_and_record_stay_consistent_under_churn() {
    let (store, _d) = open(3).await;
    store.record_turn("keep", "q", "keep-answer").await.unwrap();

    let mut handles = Vec::new();
    // Churners: push fresh sessions past the cap of 3, forcing continuous eviction.
    for i in 0..8 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..6 {
                s.record_turn(&format!("churn-{i}-{j}"), "q", "a")
                    .await
                    .expect("record under churn must not error");
            }
        }));
    }
    // Keepers: interleave record + replay on "keep". Every replay that returns turns must
    // return a clean oldest-first run of the recorded answer — never a torn/partial read.
    for _ in 0..4 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..6 {
                s.record_turn("keep", "q", "keep-answer")
                    .await
                    .expect("record under churn must not error");
                let h = s
                    .replay("keep")
                    .await
                    .expect("replay under churn must not error");
                assert!(
                    h.iter().all(|t| t.answer == "keep-answer"),
                    "a replayed history must be a consistent snapshot, never a torn read"
                );
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    // The store is internally consistent: the cap is honored despite the concurrent churn.
    assert!(store.session_count().await.unwrap() <= 3);
}

/// The concurrency verdict, as a test. kaibo runs consult jobs in concurrent spawned
/// tasks, all sharing one cloned store. A single turso Connection refuses concurrent use,
/// so the store mints a connection per op — this proves that holds up: many tasks writing
/// distinct sessions at once all succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_from_cloned_stores_all_succeed() {
    let (store, _d) = open(64).await;
    let mut handles = Vec::new();
    for i in 0..16 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let id = format!("sess-{i}");
            s.record_turn(&id, "q", "a").await
        }));
    }
    for h in handles {
        h.await
            .unwrap()
            .expect("concurrent record_turn must succeed");
    }
    assert_eq!(store.session_count().await.unwrap(), 16);
}

// --- Send verdict: kaibo's handler requires Send futures ---------------------

fn assert_send<T: Send>(_t: T) {}

/// Compile-time proof that the store's async calls return `Send` futures — the exact
/// property kaibo's rig-driven handler requires (its futures must be `Send`). If any call
/// returned a `!Send` future this test would fail to *compile*.
#[tokio::test]
async fn store_futures_are_send() {
    let (store, _d) = open(4).await;
    assert_send(store.record_turn("s", "q", "a"));
    assert_send(store.replay("s"));
    assert_send(store.list_sessions());
    assert_send(store.session_count());
    assert_send(store.put_batch("b", "p", None, None));
    assert_send(store.get_batch("b", "p"));
    assert_send(store.list_batches());
    // And the open future itself, plus the store handle as a shared handler cache.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("s.db");
    assert_send(SessionStore::open(&path, cap(4), &[]));
    fn assert_send_sync<T: Send + Sync + Clone>(_t: &T) {}
    assert_send_sync(&store);
    // Actually drive one so the futures above aren't dead-code-eliminated pre-typeck.
    store.record_turn("s", "q", "a").await.unwrap();
    let _: PathBuf = Path::new("/x").to_path_buf();
}

// ---------------------------------------------------------------------------------------
// On-disk compatibility with a state db written by an older turso (the pin bump gate).
// ---------------------------------------------------------------------------------------

/// The content the fixture generator writes and the compat test expects, in one place.
const FIXTURE_SESSION: &str = "fixture-session";
const FIXTURE_BATCH: (&str, &str, &str) = ("anthropic", "msgbatch_fixture", "fixture-label");

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/turso-0.7.2")
}

/// GENERATOR for `tests/fixtures/turso-0.7.2/`; the full recipe is in that directory's
/// README. Run ONLY with the turso 0.7.2 writer code checked out
/// (`git checkout 280ef98 -- Cargo.toml Cargo.lock src/store.rs`), with `SCHEMA_VERSION`
/// still 2. Run under any newer pin it silently produces a newer-version fixture and the
/// compatibility test then proves nothing. It writes through the real [`SessionStore`]
/// and copies the files while the store is STILL OPEN, so the copy carries a live `-wal`
/// and `-tshm` the way a killed MCP server leaves them (no clean-close checkpoint).
#[tokio::test]
#[ignore = "generator: run by hand under the old turso pin, see the doc comment"]
async fn generate_turso_fixture() {
    let out = PathBuf::from(std::env::var("KAIBO_FIXTURE_OUT").expect("KAIBO_FIXTURE_OUT"));
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    store.record_turn(FIXTURE_SESSION, "q1", "a1 ünï").await.unwrap();
    store.record_turn(FIXTURE_SESSION, "q2", "a2").await.unwrap();
    store.record_turn("other", "oq", "oa").await.unwrap();
    // A fixed far-future `created_at` (year 2100) so the open-time 30-day TTL prune can
    // never expire the handle; the timestamp is not what the fixture proves.
    store
        .put_batch_at(
            FIXTURE_BATCH.0,
            FIXTURE_BATCH.1,
            Some(FIXTURE_BATCH.2),
            Some(7),
            4_102_444_800,
        )
        .await
        .unwrap();
    std::fs::create_dir_all(&out).unwrap();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), out.join(entry.file_name())).unwrap();
    }
    drop(store);
}

/// A state db written by turso 0.7.2 (live WAL and `-tshm` sidecars included) must open
/// under the current pin and read back every session and batch handle, and accept writes.
///
/// Unix 64-bit only: the fixture was written with multiprocess WAL, and kaibo opens
/// non-multiprocess elsewhere (Windows), which against this file is exactly the forbidden
/// mixed open.
#[cfg(all(unix, target_pointer_width = "64"))]
#[tokio::test]
async fn opens_a_state_db_written_by_turso_0_7_2() {
    let dir = TempDir::new().unwrap();
    let mut copied = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "README.md" {
            continue;
        }
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        copied.push(entry.file_name().to_string_lossy().into_owned());
    }
    assert!(copied.iter().any(|n| n == "state.db"), "fixture has state.db: {copied:?}");
    let path = dir.path().join("state.db");
    // The premise: all data sits in an uncheckpointed WAL over a one-page db. A
    // checkpointed or regenerated fixture would pass while proving nothing.
    let db_len = std::fs::metadata(&path).unwrap().len();
    let wal_len = std::fs::metadata(dir.path().join("state.db-wal")).unwrap().len();
    assert_eq!(db_len, 4096, "fixture state.db must be one page; regenerate per its README");
    assert!(wal_len > 0, "fixture state.db-wal must be non-empty (uncheckpointed)");
    let store = SessionStore::open(&path, cap(8), &[])
        .await
        .unwrap_or_else(|e| panic!("0.7.2 fixture must open (files {copied:?}): {e}"));
    assert_eq!(
        store.replay(FIXTURE_SESSION).await.unwrap(),
        vec![turn("q1", "a1 ünï"), turn("q2", "a2")]
    );
    assert_eq!(store.replay("other").await.unwrap(), vec![turn("oq", "oa")]);
    let h = store
        .get_batch(FIXTURE_BATCH.0, FIXTURE_BATCH.1)
        .await
        .unwrap()
        .expect("batch handle from the old file");
    assert_eq!(h.label.as_deref(), Some(FIXTURE_BATCH.2));
    assert_eq!(h.submitted_count, Some(7));
    // And it stays writable, and the new write survives another reopen.
    store.record_turn(FIXTURE_SESSION, "q3", "a3").await.unwrap();
    drop(store);
    let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    assert_eq!(store.replay(FIXTURE_SESSION).await.unwrap().len(), 3);
}

// --- Sign-in grants and the refresh lease ------------------------------------------

fn creds_path(dir: &TempDir) -> PathBuf {
    dir.path().join("kaibo/credentials/creds.db")
}

/// A fresh credentials db under a temp dir, plus the dir.
async fn open_creds() -> (CredentialStore, TempDir) {
    let dir = TempDir::new().unwrap();
    let store = CredentialStore::open(&creds_path(&dir), &[])
        .await
        .expect("open credentials db");
    (store, dir)
}

fn grant(backend: &str) -> kaibo::oauth::Grant {
    kaibo::oauth::Grant {
        backend: backend.into(),
        auth: kaibo::oauth::AuthProfile::Chatgpt,
        client_id: Some("oaiapp_1".into()),
        token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
        access_token: "at-1".into(),
        refresh_token: Some("rt-1".into()),
        expires_at: Some(1_000),
        account: Some("a@example.com".into()),
        scopes: Some("openid".into()),
    }
}

#[tokio::test]
async fn grants_round_trip_replace_and_delete() {
    let (store, _d) = open_creds().await;
    assert!(store.get_grant("plan").await.unwrap().is_none());
    store.put_grant(&grant("plan")).await.unwrap();
    assert_eq!(store.get_grant("plan").await.unwrap(), Some(grant("plan")));
    let mut again = grant("plan");
    again.access_token = "at-2".into();
    store.put_grant(&again).await.unwrap();
    assert_eq!(store.list_grants().await.unwrap(), vec![again]);
    assert!(store.delete_grant("plan").await.unwrap());
    assert!(!store.delete_grant("plan").await.unwrap());
}

#[tokio::test]
async fn one_holder_at_a_time_takes_the_refresh_lease() {
    let (store, _d) = open_creds().await;
    store.put_grant(&grant("plan")).await.unwrap();
    assert!(store
        .claim_refresh_lease("plan", "a", 100, 60)
        .await
        .unwrap());
    assert!(
        !store
            .claim_refresh_lease("plan", "b", 120, 60)
            .await
            .unwrap(),
        "held until 160"
    );
    // A finish from a non-holder changes nothing.
    let mut fresh = grant("plan");
    fresh.access_token = "at-b".into();
    assert!(!store.finish_refresh("b", &fresh).await.unwrap());
    // The lapsed lease is taken over, and the old holder's late finish is discarded.
    assert!(store
        .claim_refresh_lease("plan", "b", 161, 60)
        .await
        .unwrap());
    let mut late = grant("plan");
    late.access_token = "at-a".into();
    assert!(!store.finish_refresh("a", &late).await.unwrap());
    assert!(store.finish_refresh("b", &fresh).await.unwrap());
    assert_eq!(
        store.get_grant("plan").await.unwrap().unwrap().access_token,
        "at-b"
    );
    // Finishing released it.
    assert!(store
        .claim_refresh_lease("plan", "c", 162, 60)
        .await
        .unwrap());
    store.release_refresh_lease("plan", "c").await.unwrap();
    assert!(store
        .claim_refresh_lease("plan", "d", 162, 60)
        .await
        .unwrap());
}

/// A refresh that finds its grant dead removes it only while it still holds the lease.
/// `kaibo login` replacing the row clears the lease, so a refresh that started on the
/// old grant cannot delete the new sign-in when its `invalid_grant` arrives late.
#[tokio::test]
async fn a_stale_refresh_cannot_delete_a_newer_sign_in() {
    let (store, _d) = open_creds().await;
    store.put_grant(&grant("plan")).await.unwrap();
    assert!(store
        .claim_refresh_lease("plan", "old-refresh", 100, 60)
        .await
        .unwrap());
    // The operator signs in again while that refresh is in flight.
    let mut newer = grant("plan");
    newer.access_token = "at-new".into();
    newer.refresh_token = Some("rt-new".into());
    store.put_grant(&newer).await.unwrap();
    assert!(
        !store
            .delete_grant_held("plan", "old-refresh")
            .await
            .unwrap(),
        "the old holder's lease was cleared by the new sign-in"
    );
    assert_eq!(store.get_grant("plan").await.unwrap(), Some(newer));

    // The current holder can remove a dead grant.
    assert!(store
        .claim_refresh_lease("plan", "current", 100, 60)
        .await
        .unwrap());
    assert!(store.delete_grant_held("plan", "current").await.unwrap());
    assert!(store.get_grant("plan").await.unwrap().is_none());
}

#[tokio::test]
async fn the_host_id_is_created_once_and_kept() {
    let (store, dir) = open_creds().await;
    let id = store.host_id().await.unwrap();
    assert!(id.starts_with("urn:uuid:"), "{id}");
    assert_eq!(store.host_id().await.unwrap(), id);
    drop(store);
    let reopened = CredentialStore::open(&creds_path(&dir), &[]).await.unwrap();
    assert_eq!(reopened.host_id().await.unwrap(), id, "survives a reopen");
}

// --- The credentials db's directory ------------------------------------------------

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// An absent credentials directory is created at 700, whatever the umask lets the
/// state dir above it be.
#[cfg(unix)]
#[tokio::test]
async fn the_credentials_dir_is_created_private() {
    let (_store, dir) = open_creds().await;
    assert_eq!(mode(&dir.path().join("kaibo/credentials")), 0o700);
}

/// An existing credentials directory open to group or others is changed to 700 before
/// any credential is written into it.
#[cfg(unix)]
#[tokio::test]
async fn an_open_credentials_dir_is_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let creds = dir.path().join("kaibo/credentials");
    std::fs::create_dir_all(&creds).unwrap();
    std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o755)).unwrap();
    CredentialStore::open(&creds_path(&dir), &[]).await.unwrap();
    assert_eq!(mode(&creds), 0o700);
}

/// A credentials path that is a symlink is refused: kaibo would be making some other
/// directory private, or storing tokens where it cannot.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_credentials_dir_is_refused() {
    let dir = TempDir::new().unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(dir.path().join("kaibo")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.path().join("kaibo/credentials")).unwrap();
    let err = CredentialStore::open(&creds_path(&dir), &[])
        .await
        .err()
        .expect("a symlink is refused");
    assert!(matches!(err, StoreError::CredentialsDir(_)), "{err}");
    assert!(err.to_string().contains("symlink"), "{err}");
}

/// An ancestor another user could write (not sticky) is refused before the credentials
/// directory is created or changed: that user could swap it for a symlink mid-check.
#[cfg(unix)]
#[tokio::test]
async fn a_credentials_dir_under_a_shared_writable_ancestor_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = shared.join("kaibo/credentials/creds.db");
    let err = CredentialStore::open(&path, &[])
        .await
        .err()
        .expect("refused");
    assert!(matches!(err, StoreError::CredentialsDir(_)), "{err}");
    assert!(err.to_string().contains("writable by other users"), "{err}");
    assert!(!shared.join("kaibo/credentials").exists(), "nothing created under it");
}

/// A path through a symlink in a shared writable directory is resolved once, checked,
/// and used resolved from then on: swapping the link afterwards cannot move the
/// credentials db, because kaibo never goes through the link again.
#[cfg(unix)]
#[tokio::test]
async fn the_credentials_db_is_opened_at_the_checked_path_not_through_a_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let safe = dir.path().join("safe");
    let decoy = dir.path().join("decoy");
    std::fs::create_dir_all(&safe).unwrap();
    std::fs::create_dir_all(&decoy).unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let link = shared.join("state-link");
    std::os::unix::fs::symlink(&safe, &link).unwrap();
    let store = CredentialStore::open(&link.join("kaibo/credentials/creds.db"), &[])
        .await
        .expect("a link into a safe tree resolves to it");
    // The other user swaps the link; the open store keeps writing where it was checked.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&decoy, &link).unwrap();
    store.put_grant(&grant("plan")).await.unwrap();
    assert!(safe.join("kaibo/credentials/creds.db").exists());
    assert!(!decoy.join("kaibo").exists(), "nothing follows the swapped link");
}

/// A sticky shared ancestor (the `/tmp` shape) is fine: others cannot rename what they
/// do not own there.
#[cfg(unix)]
#[tokio::test]
async fn a_sticky_shared_ancestor_is_accepted() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let shared = dir.path().join("sticky");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    CredentialStore::open(&shared.join("kaibo/credentials/creds.db"), &[])
        .await
        .expect("sticky ancestor is accepted");
}

/// A `creds.db` that is a symlink is refused: the private directory would not cover
/// where the tokens land.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_credentials_db_file_is_refused() {
    let dir = TempDir::new().unwrap();
    let creds = dir.path().join("kaibo/credentials");
    std::fs::create_dir_all(&creds).unwrap();
    let outside = dir.path().join("outside.db");
    std::os::unix::fs::symlink(&outside, creds.join("creds.db")).unwrap();
    let err = CredentialStore::open(&creds_path(&dir), &[])
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("is a symlink"), "{err}");
    assert!(!outside.exists(), "nothing written through the link");
}

/// A `creds.db` with a second hard link is refused: the other name, possibly outside the
/// private directory, would still reach the tokens.
#[cfg(unix)]
#[tokio::test]
async fn a_hard_linked_credentials_db_file_is_refused() {
    let dir = TempDir::new().unwrap();
    let creds = dir.path().join("kaibo/credentials");
    std::fs::create_dir_all(&creds).unwrap();
    let outside = dir.path().join("outside.db");
    std::fs::write(&outside, b"").unwrap();
    std::fs::hard_link(&outside, creds.join("creds.db-wal")).unwrap();
    let err = CredentialStore::open(&creds_path(&dir), &[])
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("hard links"), "{err}");
}

/// The credentials db obeys the same containment rule as the state db.
#[tokio::test]
async fn the_credentials_db_is_refused_inside_an_allowed_tree() {
    let project = TempDir::new().unwrap();
    let path = project.path().join("kaibo/credentials/creds.db");
    let err = CredentialStore::open(&path, &[project.path()])
        .await
        .err()
        .expect("refused inside the project");
    assert!(matches!(err, StoreError::PathInAllowedTree(_)), "{err}");
    assert!(!project.path().join("kaibo").exists(), "refused before touching the disk");
}

/// One file cannot be both dbs: the schemas would mix in one file.
#[tokio::test]
async fn one_file_cannot_be_both_the_state_db_and_the_credentials_db() {
    let (_store, dir) = open(4).await;
    let err = CredentialStore::open(&dir.path().join("state.db"), &[])
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("already open as kaibo's state db"), "{err}");
}
