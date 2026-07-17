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
