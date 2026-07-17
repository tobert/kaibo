//! Behavioral tests for the persistent [`kaibo::store::SessionStore`].
//!
//! Two jobs: prove parity with the in-memory `session.rs` semantics (capacity-LRU, no
//! TTL, touch-on-read-and-write promotion, clone-sharing, order preservation) on a durable
//! file, and prove the persistence-specific and invariant-amendment properties (survives
//! reopen, migration idempotence, batch put/get/list/upsert, the allowed-tree containment
//! guard with symlink teeth, concurrent writes, and Send futures).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use kaibo::store::{SessionStore, StoreError, Turn};
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
            .put_batch("anthropic", "msgbatch_123", Some("nightly"))
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
        .put_batch("gemini", "batches/abc", None)
        .await
        .unwrap();
    store
        .put_batch("anthropic", "msgbatch_1", Some("hard-q"))
        .await
        .unwrap();
    // Upsert: same key, new label — one row, updated.
    store
        .put_batch("anthropic", "msgbatch_1", Some("hard-q-v2"))
        .await
        .unwrap();

    let g = store
        .get_batch("gemini", "batches/abc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g.label, None);
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
        .put_batch_at("anthropic", "old", Some("stale"), now - 40 * day)
        .await
        .unwrap();
    store
        .put_batch_at("anthropic", "new", Some("fresh"), now - day)
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
    assert_send(store.put_batch("b", "p", None));
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

// --- Local batch jobs (schema v2) -------------------------------------------

use kaibo::attach::Attachment;
use kaibo::store::{CancelLocalOutcome, LocalJobStatus};

#[tokio::test]
async fn local_enqueue_and_get_round_trips_captured_attachment_content() {
    let (store, _d) = open(8).await;
    // Two attachments — one text, one image — captured BY CONTENT at submit.
    let atts = vec![
        Attachment::Text {
            path: "notes.md".into(),
            body: "hello".into(),
        },
        Attachment::Image {
            path: "shot.png".into(),
            mime: "image/png",
            data_b64: "AAAA".into(),
        },
    ];
    let id = store
        .enqueue_local(
            "deepseek",
            Some("some-model"),
            Some("deepseek"),
            &atts,
            &["p0".to_string(), "p1".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(id, 1, "first local job id is 1");

    let job = store.get_local(id).await.unwrap().expect("job exists");
    assert_eq!(job.cast, "deepseek");
    assert_eq!(job.model.as_deref(), Some("some-model"));
    assert_eq!(job.backend.as_deref(), Some("deepseek"));
    assert_eq!(job.status, LocalJobStatus::Pending);
    // Attachment content survives the db round-trip exactly (the &'static mime re-interned).
    assert_eq!(job.attachments, atts);
    assert_eq!(job.items.len(), 2);
    assert_eq!(job.items[0].prompt, "p0");
    assert!(job.items[0].result.is_none(), "a fresh item has no result");
}

#[tokio::test]
async fn ids_are_monotone_and_list_is_newest_first() {
    let (store, _d) = open(8).await;
    let a = store
        .enqueue_local("c", None, None, &[], &["a".to_string()])
        .await
        .unwrap();
    let b = store
        .enqueue_local("c", None, None, &[], &["b".to_string()])
        .await
        .unwrap();
    assert!(b > a, "ids increase monotonically");
    let list = store.list_local().await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, b, "list is newest-first");
    assert_eq!(list[1].id, a);
    assert_eq!(list[0].total, 1);
    assert_eq!(list[0].done, 0);
}

/// The claim-race property with teeth: two separate stores on ONE db file both try to claim
/// the single pending job — exactly one wins (the `BEGIN IMMEDIATE` flip). With a deferred
/// transaction both would read `pending` and double-claim; this asserts they can't.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_next_local_is_exclusive_across_two_stores_on_one_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    let a = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    let b = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    let id = a
        .enqueue_local("c", None, None, &[], &["only".to_string()])
        .await
        .unwrap();

    // Two workers race for the one pending job.
    let ha = {
        let a = a.clone();
        tokio::spawn(async move { a.claim_next_local().await.unwrap() })
    };
    let hb = {
        let b = b.clone();
        tokio::spawn(async move { b.claim_next_local().await.unwrap() })
    };
    let (ra, rb) = (ha.await.unwrap(), hb.await.unwrap());
    let claims: Vec<i64> = [ra, rb].into_iter().flatten().collect();
    assert_eq!(
        claims,
        vec![id],
        "exactly one worker may claim the single pending job (no double-run)"
    );
    // It's running now; a further claim finds nothing.
    assert_eq!(
        a.local_status(id).await.unwrap(),
        Some(LocalJobStatus::Running)
    );
    assert!(a.claim_next_local().await.unwrap().is_none());
}

#[tokio::test]
async fn worker_lifecycle_records_items_then_marks_done() {
    let (store, _d) = open(8).await;
    let id = store
        .enqueue_local("c", None, None, &[], &["p0".to_string(), "p1".to_string()])
        .await
        .unwrap();
    assert_eq!(store.claim_next_local().await.unwrap(), Some(id));
    // Per-item results as they land — one ok, one per-item error.
    store
        .record_local_item(id, 0, Ok("answer0".into()))
        .await
        .unwrap();
    store
        .record_local_item(id, 1, Err("model failed".into()))
        .await
        .unwrap();
    store
        .mark_local_finished(id, LocalJobStatus::Done)
        .await
        .unwrap();

    let job = store.get_local(id).await.unwrap().unwrap();
    assert_eq!(job.status, LocalJobStatus::Done);
    assert_eq!(job.items[0].result, Some(Ok("answer0".to_string())));
    assert_eq!(job.items[1].result, Some(Err("model failed".to_string())));
    let sum = &store.list_local().await.unwrap()[0];
    assert_eq!(sum.done, 2);
    assert_eq!(sum.failed, 1);
}

#[tokio::test]
async fn cancel_pending_marks_cancelled_and_dequeues_it() {
    let (store, _d) = open(8).await;
    let id = store
        .enqueue_local("c", None, None, &[], &["p".to_string()])
        .await
        .unwrap();
    assert_eq!(
        store.cancel_local(id).await.unwrap(),
        CancelLocalOutcome::CancelledPending
    );
    assert_eq!(
        store.local_status(id).await.unwrap(),
        Some(LocalJobStatus::Cancelled)
    );
    // A cancelled job is no longer claimable.
    assert!(store.claim_next_local().await.unwrap().is_none());
    // Idempotent.
    assert_eq!(
        store.cancel_local(id).await.unwrap(),
        CancelLocalOutcome::AlreadyCancelled
    );
}

#[tokio::test]
async fn cancel_running_wins_over_a_late_finalize() {
    let (store, _d) = open(8).await;
    let id = store
        .enqueue_local("c", None, None, &[], &["p".to_string()])
        .await
        .unwrap();
    assert_eq!(store.claim_next_local().await.unwrap(), Some(id));
    // Operator cancels the running job.
    assert_eq!(
        store.cancel_local(id).await.unwrap(),
        CancelLocalOutcome::CancellingRunning
    );
    // A worker's finalize is guarded (WHERE status='running'), so the cancel WINS.
    store
        .mark_local_finished(id, LocalJobStatus::Done)
        .await
        .unwrap();
    assert_eq!(
        store.local_status(id).await.unwrap(),
        Some(LocalJobStatus::Cancelled),
        "a concurrent cancel must not be clobbered by the worker's Done finalize"
    );
}

#[tokio::test]
async fn cancel_unknown_and_finished() {
    let (store, _d) = open(8).await;
    assert_eq!(
        store.cancel_local(999).await.unwrap(),
        CancelLocalOutcome::Unknown
    );
    let id = store
        .enqueue_local("c", None, None, &[], &["p".to_string()])
        .await
        .unwrap();
    store.claim_next_local().await.unwrap();
    store
        .mark_local_finished(id, LocalJobStatus::Done)
        .await
        .unwrap();
    assert_eq!(
        store.cancel_local(id).await.unwrap(),
        CancelLocalOutcome::AlreadyFinished
    );
}

#[tokio::test]
async fn local_jobs_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    let id = {
        let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
        let id = store
            .enqueue_local("c", None, None, &[], &["p".to_string()])
            .await
            .unwrap();
        store.claim_next_local().await.unwrap();
        store
            .record_local_item(id, 0, Ok("durable".into()))
            .await
            .unwrap();
        store
            .mark_local_finished(id, LocalJobStatus::Done)
            .await
            .unwrap();
        id
    };
    // Reopen: the job and its captured result are still there (the db is the mailbox).
    let store = SessionStore::open(&path, cap(8), &[]).await.unwrap();
    let job = store.get_local(id).await.unwrap().unwrap();
    assert_eq!(job.status, LocalJobStatus::Done);
    assert_eq!(job.items[0].result, Some(Ok("durable".to_string())));
}
