//! The MCP logging bridge — kaibo's `tracing` events, mirrored onto the MCP
//! `notifications/message` channel so a connected client can watch the server's
//! logs without scraping stderr.
//!
//! Why a bridge and not hand-placed log calls: kaibo already logs through
//! `tracing` (startup, provider info, errors). Mirroring that one stream keeps MCP
//! logging *complete and automatic* — every `tracing::info!`/`warn!`/`error!` is
//! already a candidate — and leaves stderr untouched (the MCP channel is additive).
//! The client controls verbosity with `logging/setLevel`; that lives in a shared
//! atomic the drain reads per record (see [`server`](crate::server)).
//!
//! Shape: a [`McpBridgeLayer`] sits in the subscriber stack and forwards
//! kaibo-target events into an unbounded channel; after `serve()` a [`drain`] task
//! holds the one stdio peer and turns each record into a notification, gated by the
//! client's level. The channel decouples the *sync* `tracing` layer from the
//! *async* `peer.notify_*` — and buffers the handful of startup logs emitted before
//! the peer exists, so the client sees them once draining begins.
//!
//! v1 boundary: the global `EnvFilter` (RUST_LOG / `server.log`) gates events
//! before any layer sees them, so MCP verbosity can't exceed stderr's. Fine while
//! the operator drives both with one filter; a per-layer filter is the upgrade if
//! someone needs MCP-debug over an info stderr.
//!
//! **kaibo's level convention — audience, not severity.** Within the `kaibo` target
//! tree, levels route a record to *who needs it*, not how dire it is:
//! - **Error** — a real kaibo error: to the watching client, to the calling model's
//!   `wait` drain, and to stderr.
//! - **Warn = "the calling model should see this"** — salient/actionable (a job
//!   finished or failed, the research budget ran out), *not* a dire warning. This is
//!   the level `wait` returns to the model by default. Any kaibo code (and kaish, via
//!   a future hook) marks something for the model's attention by emitting it at Warn.
//! - **Info** — the watchable narrative: each kaish command, sweep, and milestone (see
//!   [`TracingSink`](crate::progress::TracingSink)). The user's live "watch it work"
//!   view; the model only pulls it into `wait` on request.
//! - **Debug** — the true firehose, off by default.
//!
//! The convention is safe to assert here because [`KAIBO_TARGET`] filtering means
//! rig/reqwest's *severity*-sense `warn!`s never reach this bridge — only kaibo's own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rmcp::RoleServer;
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::Peer;
use serde_json::{Map, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Only events under this target tree are mirrored to MCP — kaibo's own logs, never
/// the chatter from `rig`/`reqwest`/`rmcp` a broad `RUST_LOG` would otherwise pull
/// in. (Also the guard against a feedback loop: the drain's own machinery never logs
/// under `kaibo`.)
const KAIBO_TARGET: &str = "kaibo";

/// The level MCP logging starts at, before the client sends `logging/setLevel`.
/// Mirrors the stderr default (`kaibo=info`) so the two channels agree out of the box.
pub const DEFAULT_LEVEL: LoggingLevel = LoggingLevel::Info;

/// Map an MCP [`LoggingLevel`] to a comparable rank — low = chatty, high = severe —
/// matching the MCP spec's ordering (debug < info < notice < warning < error <
/// critical < alert < emergency). Stored in the shared [`AtomicU8`] the client's
/// `setLevel` writes and the drain reads, so "forward iff record_rank >= set_rank"
/// is a plain integer compare.
pub fn rank(level: LoggingLevel) -> u8 {
    match level {
        LoggingLevel::Debug => 0,
        LoggingLevel::Info => 1,
        LoggingLevel::Notice => 2,
        LoggingLevel::Warning => 3,
        LoggingLevel::Error => 4,
        LoggingLevel::Critical => 5,
        LoggingLevel::Alert => 6,
        LoggingLevel::Emergency => 7,
    }
}

/// Map a `tracing::Level` to the MCP [`LoggingLevel`] we report it as. `tracing` has
/// no level finer than `DEBUG`, so `TRACE` folds into `Debug` (the client filters by
/// rank either way); the other four map one-to-one.
pub fn to_mcp_level(level: Level) -> LoggingLevel {
    match level {
        Level::ERROR => LoggingLevel::Error,
        Level::WARN => LoggingLevel::Warning,
        Level::INFO => LoggingLevel::Info,
        Level::DEBUG | Level::TRACE => LoggingLevel::Debug,
    }
}

/// A flattened `tracing` event, ready to become an MCP notification. Carries the
/// already-mapped MCP level so the drain only does an integer compare, plus the
/// structured fields (the `message` field is pulled out as the human line; the rest
/// ride as structured data).
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub level: LoggingLevel,
    pub target: String,
    pub message: String,
    pub fields: Map<String, Value>,
}

impl LogRecord {
    /// Render this record as the MCP notification payload. `data` is an object so a
    /// client can read the human `message`, the `target`, and any structured fields;
    /// `logger` names kaibo so multiplexed logs stay attributable.
    pub fn into_param(self) -> LoggingMessageNotificationParam {
        let mut data = Map::new();
        data.insert("message".to_string(), Value::String(self.message));
        data.insert("target".to_string(), Value::String(self.target));
        for (k, v) in self.fields {
            // Don't let a stray `message`/`target` field clobber the canonical ones.
            data.entry(k).or_insert(v);
        }
        LoggingMessageNotificationParam {
            level: self.level,
            logger: Some(KAIBO_TARGET.to_string()),
            data: Value::Object(data),
        }
    }
}

/// A bounded, in-memory ring of recent [`LogRecord`]s — the pull side of the bridge.
/// The same events the [`McpBridgeLayer`] streams to a watching client are teed here so
/// the calling model can *drain* them on demand via the `wait` tool, filtered to the
/// level it cares about (Warn+ by default — kaibo's "the model should see this" bar).
///
/// Capacity-bounded (oldest dropped), per session, deliver-once: a drained record is
/// removed. It is **not** load-bearing — `get`/`list` stay the authoritative source of a
/// job's state; a record that ages out just means the model uses `get`. `Clone` shares
/// one `Arc<Mutex<_>>` + `Notify`, so the layer (which pushes) and the handler (which
/// drains) hold the same ring. The mutex is only ever held for `VecDeque` ops, never
/// across an `.await`.
#[derive(Clone)]
pub struct NotificationBuffer {
    inner: Arc<std::sync::Mutex<std::collections::VecDeque<LogRecord>>>,
    notify: Arc<tokio::sync::Notify>,
    cap: usize,
}

impl NotificationBuffer {
    /// A ring holding at most `cap` records.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(cap),
            )),
            notify: Arc::new(tokio::sync::Notify::new()),
            cap,
        }
    }

    /// Append a record, dropping the oldest if at capacity, and wake one waiter. Called
    /// from the sync layer, so it never blocks or awaits. `notify_one` stores a permit if
    /// no one is waiting yet, so a push that races a `wait`'s registration isn't missed.
    fn push(&self, record: LogRecord) {
        {
            let mut q = self.lock();
            if q.len() == self.cap {
                q.pop_front();
            }
            q.push_back(record);
        }
        self.notify.notify_one();
    }

    /// Remove and return up to `limit` records at or above `floor` (a [`rank`]), oldest
    /// first; records below `floor` stay in the ring (a later lower-floor drain, or the
    /// cap, takes them). Deliver-once: a returned record is gone.
    pub fn drain(&self, floor: u8, limit: usize) -> Vec<LogRecord> {
        let mut q = self.lock();
        let mut taken = Vec::new();
        let mut kept = std::collections::VecDeque::with_capacity(q.len());
        for rec in q.drain(..) {
            if taken.len() < limit && rank(rec.level) >= floor {
                taken.push(rec);
            } else {
                kept.push_back(rec);
            }
        }
        *q = kept;
        taken
    }

    /// Seed a record straight into the ring, bypassing the `tracing` layer — for tests
    /// that need a job's completion ping present without standing up a subscriber (whose
    /// callsite-interest capture is flaky; see project memory). Same effect as a real
    /// layer push, minus the client-channel tee.
    #[cfg(test)]
    pub(crate) fn push_record(&self, record: LogRecord) {
        self.push(record);
    }

    /// Drop any buffered record carrying `job = <id>` — the completion ping a finished
    /// job pushed (`jobs.rs`, at **Warn**). Once that job is collected via `get`, the ping
    /// is stale news: left in the ring it would make the *next* `wait` return instantly on
    /// old activity instead of blocking for something new (the "`wait` returns too fast"
    /// bug). Idempotent — a ping a live `wait` already drained leaves nothing to remove —
    /// and scoped to the one id, so an *uncollected* job's ping still wakes a later `wait`.
    pub fn discard_job_pings(&self, id: &str) {
        self.lock()
            .retain(|rec| rec.fields.get("job").and_then(Value::as_str) != Some(id));
    }

    /// Block up to `timeout` for records at or above `floor`, returning as soon as any
    /// land (up to `limit`) or the deadline passes (then a final drain, possibly empty).
    /// The clean-exit contract: never an error, never an indefinite hang.
    pub async fn wait_drain(
        &self,
        timeout: std::time::Duration,
        floor: u8,
        limit: usize,
    ) -> Vec<LogRecord> {
        // The simple form: drain and return at one floor, no side-channel.
        self.wait_drain_with(timeout, floor, floor, limit, |_| {})
            .await
    }

    /// The streaming core behind [`wait_drain`]: drain every record at or above
    /// `drain_floor`, hand each to `on_drained` (the `wait` tool streams the Info-level
    /// narrative to the client's progress channel here), and *return* those at or above
    /// `return_floor` (capped at `limit`). It blocks until a returnable record lands or
    /// the deadline passes — so a default `wait` streams the narrative the whole time and
    /// returns only when something the model should act on (a completion) arrives.
    ///
    /// `drain_floor` ≤ `return_floor`: the caller drains *down to* what it streams (Info)
    /// while returning only the salient (Warn+). A drained-but-not-returned record is
    /// consumed — it was delivered via `on_drained` — so the narrative isn't left to pile
    /// up once it's been streamed.
    pub async fn wait_drain_with<F: FnMut(&LogRecord)>(
        &self,
        timeout: std::time::Duration,
        drain_floor: u8,
        return_floor: u8,
        limit: usize,
        mut on_drained: F,
    ) -> Vec<LogRecord> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut collected: Vec<LogRecord> = Vec::new();
        loop {
            for rec in self.drain(drain_floor, usize::MAX) {
                on_drained(&rec);
                if rank(rec.level) >= return_floor && collected.len() < limit {
                    collected.push(rec);
                }
            }
            if !collected.is_empty() {
                return collected;
            }
            // Register the wake future *before* re-checking, so a push between the drain
            // above and the await below leaves a permit rather than being missed.
            let notified = self.notify.notified();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                for rec in self.drain(drain_floor, usize::MAX) {
                    on_drained(&rec);
                    if rank(rec.level) >= return_floor && collected.len() < limit {
                        collected.push(rec);
                    }
                }
                return collected;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {}
            }
        }
    }

    /// Records currently buffered. For tests and the `wait` footer.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<LogRecord>> {
        self.inner
            .lock()
            .expect("notification buffer mutex poisoned")
    }
}

/// Collects an event's fields into a JSON map, pulling the special `message` field
/// out as a plain string. Everything else is captured with its `Debug`/typed value
/// so structured logging (`tracing::info!(provider = %p, …)`) survives onto the wire.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(rendered));
        }
    }
}

/// The subscriber layer: snapshot each kaibo-target event into a [`LogRecord`] and
/// push it down the channel. Non-kaibo events are skipped here so a broad `RUST_LOG`
/// doesn't flood the MCP client with dependency noise.
pub struct McpBridgeLayer {
    tx: UnboundedSender<LogRecord>,
    /// The pull-side ring, teed alongside the push channel: the same record streamed to
    /// the client is buffered for the `wait` tool to drain. `None` when no buffer is
    /// wired (e.g. a test subscriber).
    buffer: Option<NotificationBuffer>,
}

impl McpBridgeLayer {
    pub fn new(tx: UnboundedSender<LogRecord>) -> Self {
        Self { tx, buffer: None }
    }

    /// Tee each mirrored record into `buffer` as well as the client channel, so the
    /// calling model can drain it via `wait`.
    pub fn with_buffer(mut self, buffer: NotificationBuffer) -> Self {
        self.buffer = Some(buffer);
        self
    }
}

impl<S: Subscriber> Layer<S> for McpBridgeLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Exactly `kaibo` or a `kaibo::` submodule — boundary-checked so a hypothetical
        // `kaibox` target can't slip onto the bridge. (The per-layer EnvFilter already
        // scopes to `kaibo`, so this is belt-and-suspenders.)
        let target = meta.target();
        if target != KAIBO_TARGET && !target.starts_with("kaibo::") {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let record = LogRecord {
            level: to_mcp_level(*meta.level()),
            target: meta.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields: visitor.fields,
        };
        // Tee to the pull-side ring (for `wait`) before the push channel (for the
        // streaming client). Both are infallible/non-blocking — never panic in a log path.
        if let Some(buffer) = &self.buffer {
            buffer.push(record.clone());
        }
        // A closed channel means the drain task is gone (shutdown) — drop silently;
        // there is nowhere left to deliver, and we must never panic in a log path.
        let _ = self.tx.send(record);
    }
}

/// Forward buffered log records to the client, one notification per record, until
/// the channel closes (shutdown). `level` is the client's current floor (written by
/// `logging/setLevel`); a record below it is dropped here, so a runtime level change
/// takes effect immediately without touching the subscriber.
///
/// Failures to notify are swallowed: a dropped log line must never take down the
/// server, and surfacing it via `tracing` could feed straight back into this loop.
pub async fn drain(
    mut rx: UnboundedReceiver<LogRecord>,
    level: Arc<AtomicU8>,
    peer: Peer<RoleServer>,
) {
    while let Some(record) = rx.recv().await {
        if rank(record.level) < level.load(Ordering::Relaxed) {
            continue;
        }
        let _ = peer.notify_logging_message(record.into_param()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: LoggingLevel, message: &str) -> LogRecord {
        LogRecord {
            level,
            target: "kaibo::test".into(),
            message: message.into(),
            fields: Map::new(),
        }
    }

    #[test]
    fn buffer_drain_filters_by_level_and_leaves_the_rest() {
        let buf = NotificationBuffer::new(8);
        buf.push(rec(LoggingLevel::Info, "kaish a"));
        buf.push(rec(LoggingLevel::Warning, "job done"));
        buf.push(rec(LoggingLevel::Info, "kaish b"));

        // Default drain at the Warn floor takes only the promote-to-model record; the
        // Info narrative stays for a lower-floor drain.
        let warn = buf.drain(rank(LoggingLevel::Warning), 20);
        assert_eq!(warn.len(), 1);
        assert_eq!(warn[0].message, "job done");
        assert_eq!(buf.len(), 2, "the two Info records stay");

        // A later Info-floor drain gets them, oldest-first.
        let info = buf.drain(rank(LoggingLevel::Info), 20);
        let msgs: Vec<_> = info.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(msgs, vec!["kaish a", "kaish b"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_drops_oldest_past_capacity() {
        let buf = NotificationBuffer::new(2);
        buf.push(rec(LoggingLevel::Warning, "1"));
        buf.push(rec(LoggingLevel::Warning, "2"));
        buf.push(rec(LoggingLevel::Warning, "3")); // evicts "1"
        let got: Vec<_> = buf
            .drain(rank(LoggingLevel::Info), 20)
            .into_iter()
            .map(|r| r.message)
            .collect();
        assert_eq!(got, vec!["2".to_string(), "3".to_string()]);
    }

    #[tokio::test]
    async fn wait_drain_returns_when_a_record_lands() {
        let buf = NotificationBuffer::new(8);
        let pusher = buf.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            pusher.push(rec(LoggingLevel::Warning, "landed"));
        });
        let got = buf
            .wait_drain(
                std::time::Duration::from_secs(5),
                rank(LoggingLevel::Warning),
                20,
            )
            .await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "landed");
    }

    #[tokio::test]
    async fn wait_drain_times_out_clean_and_empty() {
        let buf = NotificationBuffer::new(8);
        // Only an Info record, but we wait at the Warn floor — nothing matches, so it
        // must time out cleanly (empty), never hang or error. A short real timeout keeps
        // the test fast without tokio's test-util time control.
        buf.push(rec(LoggingLevel::Info, "narrative"));
        let got = buf
            .wait_drain(
                std::time::Duration::from_millis(80),
                rank(LoggingLevel::Warning),
                20,
            )
            .await;
        assert!(got.is_empty(), "no Warn+ record ⇒ clean empty return");
        assert_eq!(
            buf.len(),
            1,
            "the unmatched Info record is left in the ring"
        );
    }

    #[tokio::test]
    async fn wait_drain_with_streams_everything_but_returns_only_the_salient() {
        let buf = NotificationBuffer::new(8);
        buf.push(rec(LoggingLevel::Info, "kaish a"));
        buf.push(rec(LoggingLevel::Warning, "job done"));
        buf.push(rec(LoggingLevel::Info, "kaish b"));

        let mut streamed = Vec::new();
        let returned = buf
            .wait_drain_with(
                std::time::Duration::from_secs(5),
                rank(LoggingLevel::Info), // drain down to Info (to stream)
                rank(LoggingLevel::Warning), // return only the Warn-bar records
                20,
                |r| streamed.push(r.message.clone()),
            )
            .await;

        // The human side (on_drained) sees the whole narrative, in order…
        assert_eq!(streamed, vec!["kaish a", "job done", "kaish b"]);
        // …while the model's return is just the promote-to-model record.
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].message, "job done");
        // Everything drained is consumed — the streamed narrative isn't left to pile up.
        assert!(buf.is_empty());
    }

    /// A finished job's completion ping (carrying `job=<id>`) is retired when that job is
    /// collected — but only that job's ping. The bug it guards: a stale ping left in the
    /// ring makes the next `wait` return instantly instead of blocking for new work.
    #[test]
    fn discard_job_pings_drops_only_the_named_jobs_ping() {
        fn ping(job: &str) -> LogRecord {
            let mut fields = Map::new();
            fields.insert("job".into(), Value::String(job.into()));
            LogRecord {
                level: LoggingLevel::Warning,
                target: "kaibo::jobs".into(),
                message: format!("async job finished — collect it with `get` ({job})"),
                fields,
            }
        }
        let buf = NotificationBuffer::new(8);
        buf.push(ping("job-1"));
        buf.push(ping("job-2"));
        buf.push(rec(LoggingLevel::Warning, "a jobless warn")); // no `job` field

        buf.discard_job_pings("job-1");

        // job-1's ping is gone; job-2's and the jobless warn survive (drain proves what's
        // left and in what order).
        let left: Vec<String> = buf
            .drain(rank(LoggingLevel::Warning), 20)
            .into_iter()
            .map(|r| r.message)
            .collect();
        assert_eq!(
            left,
            vec![
                "async job finished — collect it with `get` (job-2)".to_string(),
                "a jobless warn".to_string(),
            ]
        );
    }

    #[test]
    fn tracing_levels_map_to_mcp_levels() {
        assert_eq!(to_mcp_level(Level::ERROR), LoggingLevel::Error);
        assert_eq!(to_mcp_level(Level::WARN), LoggingLevel::Warning);
        assert_eq!(to_mcp_level(Level::INFO), LoggingLevel::Info);
        assert_eq!(to_mcp_level(Level::DEBUG), LoggingLevel::Debug);
        // TRACE has no MCP peer — it folds into Debug rather than being dropped.
        assert_eq!(to_mcp_level(Level::TRACE), LoggingLevel::Debug);
    }

    #[test]
    fn rank_orders_least_to_most_severe() {
        // The spec ordering, the contract the drain's `>=` filter relies on.
        let ascending = [
            LoggingLevel::Debug,
            LoggingLevel::Info,
            LoggingLevel::Notice,
            LoggingLevel::Warning,
            LoggingLevel::Error,
            LoggingLevel::Critical,
            LoggingLevel::Alert,
            LoggingLevel::Emergency,
        ];
        for pair in ascending.windows(2) {
            assert!(
                rank(pair[0]) < rank(pair[1]),
                "{:?} must rank below {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// The drain's gate, as a pure predicate: forward iff the record's rank is at or
    /// above the client's floor. Pinning it here means the filter logic has teeth
    /// without standing up a peer.
    fn forwards(record: LoggingLevel, floor: LoggingLevel) -> bool {
        rank(record) >= rank(floor)
    }

    #[test]
    fn level_filter_forwards_at_or_above_the_floor() {
        // Floor = Warning: warnings and errors pass; info and debug are dropped.
        assert!(forwards(LoggingLevel::Error, LoggingLevel::Warning));
        assert!(forwards(LoggingLevel::Warning, LoggingLevel::Warning));
        assert!(!forwards(LoggingLevel::Info, LoggingLevel::Warning));
        assert!(!forwards(LoggingLevel::Debug, LoggingLevel::Warning));
        // Floor = Debug (most verbose): everything passes.
        assert!(forwards(LoggingLevel::Debug, LoggingLevel::Debug));
    }

    #[test]
    fn record_renders_message_target_and_fields_into_data() {
        let mut fields = Map::new();
        fields.insert("provider".to_string(), Value::String("anthropic".into()));
        fields.insert("count".to_string(), Value::from(3));
        let param = LogRecord {
            level: LoggingLevel::Info,
            target: "kaibo::server".into(),
            message: "starting".into(),
            fields,
        }
        .into_param();

        assert_eq!(param.level, LoggingLevel::Info);
        assert_eq!(param.logger.as_deref(), Some("kaibo"));
        let data = param.data.as_object().expect("data is an object");
        assert_eq!(data["message"], Value::String("starting".into()));
        assert_eq!(data["target"], Value::String("kaibo::server".into()));
        assert_eq!(data["provider"], Value::String("anthropic".into()));
        assert_eq!(data["count"], Value::from(3));
    }

    #[test]
    fn canonical_keys_survive_a_colliding_field() {
        // A user field literally named `target` must not clobber the real target.
        let mut fields = Map::new();
        fields.insert("target".to_string(), Value::String("decoy".into()));
        let param = LogRecord {
            level: LoggingLevel::Warning,
            target: "kaibo".into(),
            message: "m".into(),
            fields,
        }
        .into_param();
        assert_eq!(
            param.data.as_object().unwrap()["target"],
            Value::String("kaibo".into())
        );
    }
}
