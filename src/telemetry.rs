//! OpenTelemetry traces export — opt-in, off by default.
//!
//! kaibo barely needs to instrument anything: rig already emits the GenAI span
//! tree from inside its agent loop — an `invoke_agent` span per phase, a `chat`
//! span per turn (carrying `gen_ai.request.model` and every `gen_ai.usage.*` token
//! field), and a `tool` span per tool call. Our `run_kaish` and delegated
//! `explore′` sweeps are rig tools, so they show up as tool spans for free; the
//! `#[instrument]`s on the four MCP handlers and on `run_phase` (see `server.rs` /
//! `consult.rs`) just give that tree named kaibo parents. This module's whole job
//! is to *export* it: stand up an OTLP/HTTP exporter and hand a
//! [`tracing_opentelemetry`] layer to `main`'s subscriber registry.
//!
//! ## Boundaries
//!
//! - **Off by default.** kaibo reads private source, and rig's spans carry
//!   prompts, completions, and source snippets. A default run must ship nothing —
//!   so [`init`] returns `Ok(None)` unless `[telemetry]` opts in. See
//!   [`crate::config::TelemetryConfig`].
//! - **stdio-only holds.** The exporter opens an *outbound* connection to the
//!   collector; it never *binds* a socket. That's the line the invariant draws.
//! - **Never the stdout channel.** Errors (a down collector, a flush timeout) go to
//!   `tracing` → stderr, never stdout (the MCP transport). The OTel layer's own
//!   filter excludes the `opentelemetry` target so the SDK's internal logs can't
//!   feed back into the exporter and loop.

use std::collections::HashMap;

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{filter::EnvFilter, Layer};

use crate::config::TelemetryConfig;

/// Owns the tracer provider so `main` can flush and shut it down on exit. The
/// batch processor buffers spans off-thread; dropping the provider without a
/// [`shutdown`](OtelGuard::shutdown) would discard whatever hasn't been exported.
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl OtelGuard {
    /// Flush buffered spans and stop the exporter. Errors are logged, not
    /// propagated: a wedged collector must never turn a clean shutdown into a
    /// non-zero exit, and there is nothing left to retry against.
    pub fn shutdown(self) {
        if let Err(e) = self.provider.shutdown() {
            tracing::warn!(error = %e, "OTLP trace exporter shutdown reported an error");
        }
    }
}

/// The boxed tracing layer paired with the guard that flushes it on shutdown —
/// what [`init`] returns once telemetry is enabled.
type OtelLayer<S> = (Box<dyn Layer<S> + Send + Sync>, OtelGuard);

/// Build the OTLP exporter and the tracing layer that feeds it, from config.
///
/// Returns `Ok(None)` when telemetry is disabled — the caller adds nothing to the
/// registry and pays zero overhead. When enabled, returns the layer to add **and**
/// an [`OtelGuard`] the caller must hold until after the server loop and then
/// [`shutdown`](OtelGuard::shutdown). Generic over the subscriber so the layer can
/// be boxed against `main`'s concrete registry type.
pub fn init<S>(cfg: &TelemetryConfig) -> Result<Option<OtelLayer<S>>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    if !cfg.enabled {
        return Ok(None);
    }

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

    let resource = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .build();

    // Batch processor: spans buffer off the hot path and export in the background.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("kaibo");

    // The fmt/MCP layers filter to the `kaibo` target — which would drop rig's
    // spans (targets `rig::agent_chat`, `rig::*`), the whole reason this layer
    // exists. So the OTel layer carries its OWN filter, admitting everything at
    // `info` while turning `opentelemetry`'s internal logs OFF: exporting those
    // could feed back into the exporter and loop.
    let filter = EnvFilter::new("info,opentelemetry=off");

    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter)
        .boxed();

    Ok(Some((layer, OtelGuard { provider })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::Registry;

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
    async fn enabled_config_builds_a_layer_and_shuts_down() {
        // Proves the exporter + provider + layer wiring constructs and tears down
        // with our chosen feature set (HTTP/protobuf, reqwest, batch). No network:
        // building the exporter does not connect, and shutdown of an empty buffer
        // doesn't require a reachable collector.
        let cfg = TelemetryConfig {
            enabled: true,
            // Unroutable on purpose — we never export here, only build + shut down.
            endpoint: "http://127.0.0.1:4318/v1/traces".to_string(),
            ..TelemetryConfig::default()
        };
        let (_layer, guard) = init::<Registry>(&cfg)
            .unwrap()
            .expect("enabled telemetry must yield a layer");
        guard.shutdown();
    }
}
