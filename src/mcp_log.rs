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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::Peer;
use rmcp::RoleServer;
use serde_json::{Map, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

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
            self.fields.insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields.insert(field.name().to_string(), Value::String(rendered));
        }
    }
}

/// The subscriber layer: snapshot each kaibo-target event into a [`LogRecord`] and
/// push it down the channel. Non-kaibo events are skipped here so a broad `RUST_LOG`
/// doesn't flood the MCP client with dependency noise.
pub struct McpBridgeLayer {
    tx: UnboundedSender<LogRecord>,
}

impl McpBridgeLayer {
    pub fn new(tx: UnboundedSender<LogRecord>) -> Self {
        Self { tx }
    }
}

impl<S: Subscriber> Layer<S> for McpBridgeLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with(KAIBO_TARGET) {
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
            assert!(rank(pair[0]) < rank(pair[1]), "{:?} must rank below {:?}", pair[0], pair[1]);
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
        assert_eq!(param.data.as_object().unwrap()["target"], Value::String("kaibo".into()));
    }
}
