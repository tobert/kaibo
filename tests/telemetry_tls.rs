//! Offline canary on the OTLP exporter's TLS wiring — the gap between the two
//! existing telemetry tests.
//!
//! `src/telemetry.rs` proves the exporter *builds* but never exports, and
//! `tests/telemetry_live.rs` exports for real but is `#[ignore]`d (needs a live
//! collector). Neither catches the failure that actually bit us: opentelemetry-otlp
//! builds its OWN reqwest (blocking) client *lazily, at first export, on a detached
//! thread*, and reads the process-default rustls provider when it does. reqwest is
//! compiled `rustls-no-provider`, so that client build panics — "No rustls crypto
//! provider is configured" — unless someone installed a provider first. The other
//! client build sites (`consult.rs`, `image_gen.rs`) call `ensure_crypto_provider`;
//! `telemetry::init` must too, or the live binary aborts on its first span export.
//!
//! This runs in its own test binary so the process-global provider install is
//! uncontaminated: nothing else here installs one, so "after init it's present"
//! genuinely fails when the install regresses.

use kaibo::config::TelemetryConfig;

#[test]
fn init_installs_the_crypto_provider_the_otlp_client_will_need() {
    // Precondition with teeth: this isolated test process owns the one provider
    // install. If something installed one before init(), the post-assert below would
    // pass for the wrong reason — catch that here.
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "guard: this test must be the only thing installing the process-global provider"
    );

    let cfg = TelemetryConfig {
        enabled: true,
        // Unroutable on purpose — we never export, only build the exporter and assert
        // the provider its lazily-built client will read is already installed.
        endpoint: "http://127.0.0.1:4318/v1/traces".to_string(),
        ..TelemetryConfig::default()
    };
    let (_layer, guard) = kaibo::telemetry::init::<tracing_subscriber::Registry>(&cfg)
        .expect("telemetry init must not error")
        .expect("an enabled config must yield a layer");

    // The teeth: otlp's reqwest client reads THIS process default at first export.
    // None here ⇒ the live binary panics on its first span. It must be ring's.
    let installed = rustls::crypto::CryptoProvider::get_default()
        .expect("init must install the process-default crypto provider the otlp client reads");
    let ring = rustls::crypto::ring::default_provider();
    assert_eq!(
        installed.cipher_suites.len(),
        ring.cipher_suites.len(),
        "the provider init installs should be ring's"
    );

    guard.shutdown();
}
