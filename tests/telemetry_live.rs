//! Live OTLP export probe — `#[ignore]`d, needs a real OTLP/HTTP collector.
//!
//! The offline tests (`src/telemetry.rs`) prove the exporter *builds* and the gate
//! is off-by-default, but they never push a span over a socket — so they can't catch
//! the failure mode that actually bit us: the default `with_batch_exporter` drives
//! export on a dedicated thread via `futures_executor::block_on` with no tokio
//! reactor, so an *async* reqwest client fails every export ("no reactor running")
//! and silently drops spans. This probe exports for real, under a `#[tokio::main]`
//! context like the binary's, so a regression there fails a run instead of going
//! quietly unnoticed.
//!
//! Run with a collector up (endpoint via `KAIBO_TELEMETRY_ENDPOINT`, else the
//! local default), then confirm the marker landed downstream:
//!   cargo test --test telemetry_live -- --ignored --nocapture
//!   # then e.g.:  grep kaibo-otel-selftest /tank/otel/traces/traces.jsonl

use kaibo::config::TelemetryConfig;
use tracing::subscriber;
use tracing_subscriber::prelude::*;

/// The unique service name to grep for downstream (collector file, otlp backend, …).
const MARKER: &str = "kaibo-otel-selftest";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live OTLP/HTTP collector on KAIBO_TELEMETRY_ENDPOINT (or :4318)"]
async fn live_export_reaches_an_otlp_collector() {
    let endpoint = std::env::var("KAIBO_TELEMETRY_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318/v1/traces".to_string());
    eprintln!("exporting a probe span as service.name={MARKER} to {endpoint}");

    let cfg = TelemetryConfig {
        enabled: true,
        endpoint,
        service_name: MARKER.to_string(),
        ..TelemetryConfig::default()
    };

    // Same path the binary takes: build the layer + guard, install the layer, emit a
    // span (rig's spans are `info_span!`s just like this), then force-flush on
    // shutdown. If export is wired wrong this silently no-ops — so the assertion
    // lives downstream (grep the collector), the way the docstring describes.
    let (layer, guard) = kaibo::telemetry::init::<tracing_subscriber::Registry>(&cfg)
        .expect("telemetry init must not error")
        .expect("an enabled config must yield a layer");

    let sub = tracing_subscriber::registry().with(layer);
    subscriber::with_default(sub, || {
        let span = tracing::info_span!(
            "kaibo_selftest_span",
            gen_ai.operation.name = "selftest",
            kaibo.probe = MARKER,
        );
        let _enter = span.enter();
        tracing::info!(marker = MARKER, "selftest span body");
    });

    // Force-flush + stop. Without the blocking-client fix this is where the dropped
    // export would (silently) happen.
    guard.shutdown();
    eprintln!("flushed; grep the collector for {MARKER}");
}
