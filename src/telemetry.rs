//! OpenTelemetry export — traces and logs, opt-in, off by default.
//!
//! kaibo barely needs to instrument anything: rig already emits the GenAI span
//! tree from inside its agent loop — an `invoke_agent` span per phase, a `chat`
//! span per turn (carrying `gen_ai.request.model` and every `gen_ai.usage.*` token
//! field), and a `tool` span per tool call. Our `run_kaish` and delegated
//! `explore′` sweeps are rig tools, so they show up as tool spans for free; the
//! `#[instrument]`s on the four MCP handlers and on `run_phase` (see `server.rs` /
//! `consult.rs`) just give that tree named kaibo parents. This module's whole job
//! is to *export* it: stand up the OTLP/HTTP exporters and hand `main`'s subscriber
//! registry the layers that feed them.
//!
//! ## Two signals, and why the second one exists
//!
//! Traces carry rig's span tree. **Logs carry kaibo's own `tracing` events**, which
//! nothing exported before — and several things worth counting are only ever events,
//! never spans. The two that asked for this: the `warn` a phase emits when a model
//! calls a tool that does not exist (`consult/engine.rs`), and the `warn` when a
//! phase returns an empty answer and is forced into a write-up turn. Both classes
//! were tracked as "incidence unmeasured" precisely because the instrument existed
//! and had nowhere to report.
//!
//! **Which events.** The traces layer admits everything at `info`, because rig's
//! spans *are* the tree. The logs layer is deliberately narrower — `kaibo=info` —
//! and that asymmetry is the design. rig also emits event-level chatter carrying
//! prompt and completion text; traces already carry that content once, in a shape a
//! backend can read, so exporting it again as loose log lines would pay twice for
//! the most sensitive bytes kaibo handles. The logs signal is scoped to what kaibo
//! says about itself.
//!
//! ## Boundaries
//!
//! - **Off by default.** kaibo reads private source, and rig's spans carry
//!   prompts, completions, and source snippets. A default run must ship nothing —
//!   so [`init`] returns `Ok(None)` unless `[telemetry]` opts in. See
//!   [`crate::config::TelemetryConfig`].
//! - **One opt-in covers both signals.** `logs` defaults on *under* `enabled`,
//!   because kaibo's own diagnostics are strictly less sensitive than the prompts
//!   the traces already carry — an operator who accepted the first has no new
//!   disclosure to weigh. `logs = false` is there for a collector that takes spans
//!   but not records.
//! - **The logs endpoint is derived only from the standard path shape**, never
//!   guessed. See [`resolve_logs_endpoint`] — a misroute would be discovered only by
//!   the records' absence, which is the silent failure this house refuses.
//! - **stdio-only holds.** The exporters open *outbound* connections to the
//!   collector; they never *bind* a socket. That's the line the invariant draws.
//! - **Never the stdout channel.** Errors (a down collector, a flush timeout) go to
//!   `tracing` → stderr, never stdout (the MCP transport). Both layers' filters
//!   exclude the `opentelemetry` target so the SDK's internal logs can't feed back
//!   into the exporter and loop.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{filter::EnvFilter, Layer};

use crate::config::TelemetryConfig;

/// The standard OTLP/HTTP path pair. Deriving one from the other is only honest for
/// exactly this shape; anything else is the operator's to state.
const TRACES_PATH: &str = "/v1/traces";
const LOGS_PATH: &str = "/v1/logs";

/// What the logs layer admits. Narrower than the traces layer's `info` — see the
/// module docs' "Which events" note for why the model stack is left out.
const LOGS_FILTER: &str = "kaibo=info,opentelemetry=off";

/// Owns the providers so `main` can flush and shut them down on exit. The batch
/// processor buffers records off-thread; dropping a provider without a
/// [`shutdown`](OtelGuard::shutdown) would discard whatever hasn't been exported.
pub struct OtelGuard {
    traces: SdkTracerProvider,
    /// `None` when `[telemetry] logs = false` — the signal was never stood up.
    logs: Option<SdkLoggerProvider>,
}

impl OtelGuard {
    /// Flush buffered spans and records, then stop the exporters. Errors are logged,
    /// not propagated: a wedged collector must never turn a clean shutdown into a
    /// non-zero exit, and there is nothing left to retry against. Both signals are
    /// shut down even if the first reports an error — one bad collector must not
    /// strand the other signal's buffer.
    pub fn shutdown(self) {
        if let Err(e) = self.traces.shutdown() {
            tracing::warn!(error = %e, "OTLP trace exporter shutdown reported an error");
        }
        if let Some(logs) = self.logs {
            if let Err(e) = logs.shutdown() {
                tracing::warn!(error = %e, "OTLP log exporter shutdown reported an error");
            }
        }
    }
}

/// The boxed tracing layers paired with the guard that flushes them on shutdown —
/// what [`init`] returns once telemetry is enabled. A `Vec` because the count varies
/// with `[telemetry] logs`, and `tracing-subscriber` layers a `Vec` as one unit.
type OtelLayers<S> = (Vec<Box<dyn Layer<S> + Send + Sync>>, OtelGuard);

/// Where the logs signal exports to: the explicit `logs_endpoint`, else the standard
/// sibling of a standard traces endpoint.
///
/// The refusal in the middle is the point. A collector addressed at some other path
/// (`/otlp/ingest`, a vendor's ingest URL) gives kaibo no way to know its logs route,
/// and inventing one would ship kaibo's diagnostics to a URL the operator never chose
/// — a silent misroute, discovered only by their absence. So it fails at load and
/// names the key that fixes it.
pub(crate) fn resolve_logs_endpoint(cfg: &TelemetryConfig) -> Result<String> {
    if let Some(explicit) = &cfg.logs_endpoint {
        return Ok(explicit.clone());
    }
    match cfg.endpoint.strip_suffix(TRACES_PATH) {
        Some(base) => Ok(format!("{base}{LOGS_PATH}")),
        None => bail!(
            "[telemetry] endpoint `{}` does not end in `{TRACES_PATH}`, so the logs \
             endpoint cannot be derived from it. Set [telemetry] logs_endpoint to the \
             collector's OTLP/HTTP logs URL, or set logs = false to export traces only. \
             Run `kaibo example-config` for the shape.",
            cfg.endpoint
        ),
    }
}

/// The logs layer alone, over an already-built provider — the seam the filter test
/// drives with an in-memory exporter instead of a live collector.
fn logs_layer<S>(provider: &SdkLoggerProvider) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    OpenTelemetryTracingBridge::new(provider)
        .with_filter(EnvFilter::new(LOGS_FILTER))
        .boxed()
}

/// Build the OTLP exporter and the tracing layer that feeds it, from config.
///
/// Returns `Ok(None)` when telemetry is disabled — the caller adds nothing to the
/// registry and pays zero overhead. When enabled, returns the layer to add **and**
/// an [`OtelGuard`] the caller must hold until after the server loop and then
/// [`shutdown`](OtelGuard::shutdown). Generic over the subscriber so the layer can
/// be boxed against `main`'s concrete registry type.
pub fn init<S>(cfg: &TelemetryConfig) -> Result<Option<OtelLayers<S>>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    if !cfg.enabled {
        return Ok(None);
    }

    // Resolve before building anything: a logs endpoint we cannot derive is an
    // operator mistake, and it should surface as a load error rather than after a
    // tracer provider is already standing.
    let logs_endpoint = if cfg.logs {
        Some(resolve_logs_endpoint(cfg)?)
    } else {
        None
    };

    // opentelemetry-otlp builds its own reqwest (blocking) client when we build the
    // exporter below, and — because reqwest is compiled `rustls-no-provider` (see
    // Cargo.toml / src/tls.rs) — that build panics unless a process-default crypto
    // provider is already installed. The OTel SDK owns that client build, so it can't
    // route through `tls::https_client` like the provider clients do; it installs ring
    // directly via the same `ensure_crypto_provider` seam. Anything else would abort the
    // live binary on its first span export.
    crate::tls::ensure_crypto_provider();

    // HTTP/protobuf on the async reqwest client — reuses kaibo's reqwest 0.13 +
    // rustls (no tonic/gRPC). HttpBinary is the protobuf wire (the `/v1/traces`
    // endpoint in config points at it).
    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(cfg.endpoint.clone())
        .with_timeout(cfg.timeout)
        .with_headers(cfg.headers.clone().into_iter().collect::<HashMap<_, _>>())
        .build()
        .context("building the OTLP/HTTP span exporter")?;

    // Every span leaves through the allowlist. Wrapping the exporter — rather than
    // filtering at the call sites — is the only option available: the attributes
    // carrying prompts and tool payloads are emitted inside rig, not by kaibo. See
    // `crate::otel_filter`.
    let policy = crate::otel_filter::AttributePolicy::new(cfg.capture_content, &cfg.capture);
    // Say what is leaving, at the moment it starts leaving. An operator who enabled
    // this through an ambient OTEL_* endpoint may not have thought about kaibo at
    // all, so the line names the destination and the content policy together.
    tracing::info!(
        endpoint = %cfg.endpoint,
        policy = %policy.describe(),
        "telemetry enabled — exporting spans"
    );
    let exporter = crate::otel_filter::Filtered::new(exporter, policy);

    let resource = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .build();

    // Batch processor: spans buffer off the hot path and export in the background.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource.clone())
        .build();

    let tracer = provider.tracer("kaibo");

    // The fmt/MCP layers filter to the `kaibo` target — which would drop rig's
    // spans (targets `rig::agent_chat`, `rig::*`), the whole reason this layer
    // exists. So the OTel layer carries its OWN filter, admitting everything at
    // `info` while turning `opentelemetry`'s internal logs OFF: exporting those
    // could feed back into the exporter and loop.
    let filter = EnvFilter::new("info,opentelemetry=off");

    let mut layers: Vec<Box<dyn Layer<S> + Send + Sync>> = vec![tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter)
        .boxed()];

    // The logs signal, on the same transport and the same Resource, so a backend
    // joins a record to the span tree it happened under.
    let logs_provider = match logs_endpoint {
        Some(endpoint) => {
            let exporter = LogExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(endpoint)
                .with_timeout(cfg.timeout)
                .with_headers(cfg.headers.clone().into_iter().collect::<HashMap<_, _>>())
                .build()
                .context("building the OTLP/HTTP log exporter")?;
            let logs_provider = SdkLoggerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build();
            layers.push(logs_layer(&logs_provider));
            Some(logs_provider)
        }
        None => None,
    };

    Ok(Some((
        layers,
        OtelGuard {
            traces: provider,
            logs: logs_provider,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::Registry;

    /// An enabled config pointed somewhere unroutable — these tests build and shut
    /// down, never export, so nothing needs to be listening.
    fn enabled() -> TelemetryConfig {
        TelemetryConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:4318/v1/traces".to_string(),
            ..TelemetryConfig::default()
        }
    }

    #[test]
    fn disabled_config_installs_nothing() {
        // The teeth on local-by-default: a disabled (the built-in default) config
        // builds no exporter and no layer, so `main` adds nothing to the registry.
        let cfg = TelemetryConfig::default();
        assert!(!cfg.enabled, "guard: the default must be disabled");
        let out = init::<Registry>(&cfg).unwrap();
        assert!(out.is_none(), "disabled telemetry must install no layer");
    }

    #[tokio::test]
    async fn enabled_config_builds_both_signals_and_shuts_down() {
        // Proves the exporter + provider + layer wiring constructs and tears down
        // with our chosen feature set (HTTP/protobuf, reqwest, batch). No network:
        // building the exporter does not connect, and shutdown of an empty buffer
        // doesn't require a reachable collector.
        let cfg = enabled();
        assert!(cfg.logs, "guard: logs ride the same opt-in by default");
        let (layers, guard) = init::<Registry>(&cfg)
            .unwrap()
            .expect("enabled telemetry must yield layers");
        assert_eq!(
            layers.len(),
            2,
            "an enabled config exports both signals: traces and logs"
        );
        guard.shutdown();
    }

    #[tokio::test]
    async fn logs_can_be_declined_without_losing_traces() {
        // The escape hatch for an operator whose collector takes spans but not logs:
        // `logs = false` drops the second signal and keeps the first.
        let cfg = TelemetryConfig {
            logs: false,
            ..enabled()
        };
        let (layers, guard) = init::<Registry>(&cfg)
            .unwrap()
            .expect("traces alone still yield a layer");
        assert_eq!(layers.len(), 1, "declining logs leaves the traces layer");
        guard.shutdown();
    }

    #[test]
    fn a_nonstandard_traces_endpoint_refuses_to_guess_the_logs_endpoint() {
        // Silent fallbacks are the failure we refuse: deriving `/v1/logs` from an
        // endpoint that is not the standard `/v1/traces` would ship kaibo's logs to a
        // URL the operator never chose. The load fails instead, naming the key.
        let cfg = TelemetryConfig {
            endpoint: "http://collector.internal/otlp/ingest".to_string(),
            ..enabled()
        };
        // Matched rather than `expect_err`d: the Ok arm holds boxed layers, which are
        // not `Debug`.
        let err = match init::<Registry>(&cfg) {
            Ok(_) => panic!("a non-derivable endpoint must refuse, not guess"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("logs_endpoint"),
            "the refusal must name the key that fixes it; got: {err}"
        );
    }

    #[tokio::test]
    async fn an_explicit_logs_endpoint_is_used_as_written() {
        // Both signals can address different collectors; an explicit endpoint is never
        // second-guessed against the traces one.
        let cfg = TelemetryConfig {
            endpoint: "http://collector.internal/otlp/ingest".to_string(),
            logs_endpoint: Some("http://127.0.0.1:4318/v1/logs".to_string()),
            ..enabled()
        };
        let (layers, guard) = init::<Registry>(&cfg)
            .unwrap()
            .expect("an explicit logs endpoint satisfies the derivation");
        assert_eq!(layers.len(), 2, "both signals build");
        guard.shutdown();
    }

    #[test]
    fn the_standard_endpoint_derives_its_sibling() {
        assert_eq!(
            resolve_logs_endpoint(&enabled()).unwrap(),
            "http://127.0.0.1:4318/v1/logs",
            "the standard traces path derives the standard logs path"
        );
    }

    /// The signal's whole point, and the line that keeps it cheap: kaibo's own events
    /// are exported, and the model stack's are not.
    ///
    /// Why this filter is narrower than the traces layer's: that one admits everything
    /// at `info` on purpose — rig's spans ARE the trace tree. Events are the opposite
    /// case. rig emits event-level chatter carrying prompt and completion text, which
    /// traces already carry once in a shape a backend can read; sending it again as
    /// loose log lines costs export bytes and duplicates the most sensitive content
    /// kaibo handles. So the logs signal is scoped to the `kaibo` target: the
    /// diagnostics kaibo writes about itself, which nothing exported before.
    #[test]
    fn the_logs_signal_carries_kaibo_events_and_not_the_model_stack() {
        use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};

        // Same process-global tracing hazard the span-capture tests document: a
        // no-subscriber test elsewhere in this binary can cache `Interest::never()`
        // against a callsite we need enabled. See `test_support`.
        crate::test_support::force_multi_dispatcher();
        let _serial = crate::test_support::CAPTURE_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let sub = Registry::default().with(logs_layer::<Registry>(&provider));
        tracing::subscriber::with_default(sub, || {
            // The event this signal exists to deliver: the unknown-tool hook's warn
            // (`consult/engine.rs`), which is the instrument behind the recovery-rate
            // question and which nothing exported before.
            tracing::warn!(
                target: "kaibo::consult::engine",
                model = "deepseek-v4-pro",
                "model called a tool that does not exist"
            );
            // rig's event chatter: the prompt/completion content traces already carry.
            tracing::info!(target: "rig::agent_chat", prompt = "secret source", "chat");
            // The SDK's own logs, which would feed back into the exporter and loop.
            tracing::info!(target: "opentelemetry", "exporter internal");
        });

        provider.force_flush().expect("flush the simple exporter");
        let emitted = exporter.get_emitted_logs().expect("read exported records");
        let bodies: Vec<String> = emitted
            .iter()
            .map(|l| format!("{:?}", l.record.body()))
            .collect();

        assert_eq!(
            emitted.len(),
            1,
            "exactly kaibo's own event is exported; got: {bodies:?}"
        );
        assert!(
            bodies[0].contains("tool that does not exist"),
            "the exported record is the unknown-tool warn; got: {bodies:?}"
        );
    }
}
