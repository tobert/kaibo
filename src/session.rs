//! In-memory multi-turn session store for `consult`.
//!
//! `consult` is otherwise stateless — every call re-explores the project from
//! scratch. A *session* lets a client carry the thread of a conversation across
//! calls without kaibo holding any project state: we keep only the lean
//! `(question, answer)` pairs and replay them as context on the next turn. The
//! exploration itself stays fresh each turn (the curated report is ephemeral and
//! never stored — it'd be stale bloat), mirroring dpal's session model.
//!
//! Eviction is **capacity-driven only, no TTL**: a session is dropped only when a
//! newer one pushes it past the cap as the least-recently-used. Amy holds sessions
//! open for days, so a TTL would evict a live-but-idle thread — capacity is the only
//! pressure we want. Touching a session (reading or appending) marks it
//! most-recently-used, so an actively-used thread can't be evicted out from under a
//! client that's still asking on it.
//!
//! The store is `Clone` and shares one `Arc<Mutex<_>>`, so the per-request handler
//! clones all talk to the same cache. The mutex is only ever held to read or append
//! a pair — never across an `.await` — so it can't wedge the async runtime.
//!
//! # The [`Sessions`] seam
//!
//! When persistence is enabled the server backs `consult`'s threads with the durable,
//! turso-backed [`crate::store::SessionStore`] instead of this in-memory cache — so a
//! session survives a restart and is shared across front doors (MCP ↔ CLI). [`Sessions`]
//! is the small enum that hides which backend is live behind one async `history`/`record`
//! surface, keeping `consult_session_turn` agnostic. The in-memory path is unchanged; the
//! persistent path is chosen at server construction from `[persistence]`.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;

use crate::store;

/// One completed turn: the question the client asked and the answer kaibo gave.
/// Lean by design — no exploration report, no tool transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaTurn {
    pub question: String,
    pub answer: String,
}

impl QaTurn {
    pub fn new(question: impl Into<String>, answer: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            answer: answer.into(),
        }
    }
}

/// A cap-based LRU of session histories, keyed by a client-supplied session id.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Mutex<LruCache<String, Vec<QaTurn>>>>,
}

impl SessionStore {
    /// A store holding at most `capacity` distinct sessions.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
        }
    }

    /// Snapshot the prior turns for `id`, oldest first — empty if the session is
    /// unknown. Touching the session promotes it to most-recently-used.
    pub fn history(&self, id: &str) -> Vec<QaTurn> {
        self.lock().get(id).cloned().unwrap_or_default()
    }

    /// Append a completed turn to `id`'s history, creating the session if new.
    /// Promotes `id` to most-recently-used; creating a session past the cap evicts
    /// the least-recently-used one.
    pub fn record(&self, id: &str, turn: QaTurn) {
        let mut cache = self.lock();
        if let Some(history) = cache.get_mut(id) {
            history.push(turn);
        } else {
            cache.put(id.to_string(), vec![turn]);
        }
    }

    /// Number of live sessions. For tests and diagnostics.
    pub fn session_count(&self) -> usize {
        self.lock().len()
    }

    /// A poisoned lock means a holder panicked mid-mutation — but the only work done
    /// under the lock is `Vec`/`LruCache` ops on owned strings, which don't panic, so
    /// poisoning is effectively unreachable. Treat it as the build-time bug it would
    /// be rather than masking it.
    fn lock(&self) -> std::sync::MutexGuard<'_, LruCache<String, Vec<QaTurn>>> {
        self.inner.lock().expect("session store mutex poisoned")
    }
}

/// The session backend behind `consult`'s multi-turn threads — the seam that lets the
/// server pick durable or ephemeral storage without the consult glue caring.
///
/// - [`Sessions::Memory`] is the historical in-memory LRU ([`SessionStore`]): fast, lost
///   on restart. The default when `[persistence]` is off.
/// - [`Sessions::Persistent`] is the turso-backed [`crate::store::SessionStore`]: a
///   session survives a restart and is shared across front doors (MCP ↔ CLI).
///
/// Both variants are cheap `Clone` (an `Arc` inside), so the per-request handler clones
/// share one backend. The methods are `async` because the persistent backend's are;
/// the in-memory arms complete synchronously and never error, so today's behavior is
/// byte-for-byte unchanged when `Memory` is live. A backend read/write **error
/// propagates** (it fails the turn loudly) rather than silently falling back to memory —
/// a broken store is surfaced, not papered over.
#[derive(Clone)]
pub enum Sessions {
    Memory(SessionStore),
    Persistent(store::SessionStore),
}

impl Sessions {
    /// Prior turns for `id`, oldest-first (empty if unknown). Touches the session,
    /// promoting it to most-recently-used — same contract as [`SessionStore::history`].
    pub async fn history(&self, id: &str) -> anyhow::Result<Vec<QaTurn>> {
        match self {
            Sessions::Memory(s) => Ok(s.history(id)),
            Sessions::Persistent(s) => Ok(s
                .replay(id)
                .await?
                .into_iter()
                .map(|t| QaTurn::new(t.question, t.answer))
                .collect()),
        }
    }

    /// Append a completed turn to `id`, creating the session if new and promoting it to
    /// most-recently-used — same contract as [`SessionStore::record`].
    pub async fn record(&self, id: &str, turn: QaTurn) -> anyhow::Result<()> {
        match self {
            Sessions::Memory(s) => {
                s.record(id, turn);
                Ok(())
            }
            Sessions::Persistent(s) => {
                s.record_turn(id, &turn.question, &turn.answer).await?;
                Ok(())
            }
        }
    }

    /// Number of live sessions. For tests and diagnostics.
    pub async fn session_count(&self) -> anyhow::Result<usize> {
        match self {
            Sessions::Memory(s) => Ok(s.session_count()),
            Sessions::Persistent(s) => Ok(s.session_count().await?),
        }
    }

    /// The durable store when persistence is enabled, else `None` — the handle the batch
    /// handlers use to record/recover provider batch handles in the same db. `Memory`
    /// keeps batch handles nowhere (today's behavior), so it returns `None`.
    pub fn store(&self) -> Option<&store::SessionStore> {
        match self {
            Sessions::Memory(_) => None,
            Sessions::Persistent(s) => Some(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn unknown_session_has_empty_history() {
        let store = SessionStore::new(cap(4));
        assert!(store.history("nope").is_empty());
        assert_eq!(store.session_count(), 0);
    }

    #[test]
    fn record_then_history_round_trips_and_accumulates_in_order() {
        let store = SessionStore::new(cap(4));
        store.record("s", QaTurn::new("q1", "a1"));
        store.record("s", QaTurn::new("q2", "a2"));

        let history = store.history("s");
        assert_eq!(
            history,
            vec![QaTurn::new("q1", "a1"), QaTurn::new("q2", "a2")],
            "turns must accumulate oldest-first under one session"
        );
        assert_eq!(store.session_count(), 1, "same id is one session, not two");
    }

    #[test]
    fn distinct_ids_are_distinct_sessions() {
        let store = SessionStore::new(cap(4));
        store.record("a", QaTurn::new("qa", "aa"));
        store.record("b", QaTurn::new("qb", "ab"));

        assert_eq!(store.history("a"), vec![QaTurn::new("qa", "aa")]);
        assert_eq!(store.history("b"), vec![QaTurn::new("qb", "ab")]);
        assert_eq!(store.session_count(), 2);
    }

    #[test]
    fn over_capacity_evicts_the_least_recently_used() {
        let store = SessionStore::new(cap(2));
        store.record("s1", QaTurn::new("q1", "a1"));
        store.record("s2", QaTurn::new("q2", "a2"));
        // s3 pushes past the cap of 2 → s1 (LRU) is evicted.
        store.record("s3", QaTurn::new("q3", "a3"));

        assert!(
            store.history("s1").is_empty(),
            "s1 should have been evicted"
        );
        assert_eq!(store.history("s2"), vec![QaTurn::new("q2", "a2")]);
        assert_eq!(store.history("s3"), vec![QaTurn::new("q3", "a3")]);
        assert_eq!(store.session_count(), 2);
    }

    #[test]
    fn touching_a_session_protects_it_from_eviction() {
        let store = SessionStore::new(cap(2));
        store.record("s1", QaTurn::new("q1", "a1"));
        store.record("s2", QaTurn::new("q2", "a2"));
        // Reading s1 promotes it to most-recently-used, so s2 is now the LRU.
        let _ = store.history("s1");
        store.record("s3", QaTurn::new("q3", "a3"));

        assert_eq!(
            store.history("s1"),
            vec![QaTurn::new("q1", "a1")],
            "s1 was just touched, so it must survive — eviction must follow recency"
        );
        assert!(
            store.history("s2").is_empty(),
            "s2 became the LRU and should be evicted"
        );
    }

    #[test]
    fn store_clones_share_one_cache() {
        let store = SessionStore::new(cap(4));
        let clone = store.clone();
        store.record("s", QaTurn::new("q", "a"));
        // A clone (as the per-request handler makes) sees the same session.
        assert_eq!(clone.history("s"), vec![QaTurn::new("q", "a")]);
    }
}
