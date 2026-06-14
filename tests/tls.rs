//! Canary on the TLS crypto-provider wiring — invisible to the offline harness.
//!
//! The scripted `CompletionClient` in `test_support.rs` drives the real consult
//! loop but never builds a real `reqwest::Client`, so nothing offline exercises the
//! one hop that matters here: reqwest is compiled `rustls-no-provider` (to keep
//! aws-lc-rs — C/cmake — out of the tree so static musl builds Just Work), which
//! means a client `.build()` panics at runtime unless a process-default crypto
//! provider has been installed first. A bug there passes every other test and then
//! aborts the live binary on its first model call.
//!
//! This pins the contract directly: after `ensure_crypto_provider`, building a real
//! reqwest client succeeds. It fails the moment that wiring regresses — e.g. the
//! `ensure_crypto_provider` call is dropped from a client build site, or reqwest is
//! switched back to a provider-bearing feature and then away again without an
//! installer. (That ring specifically — not aws-lc — is the provider is pinned at
//! compile time: `src/tls.rs` names `rustls::crypto::ring`, which only exists with
//! rustls's `ring` feature on and `aws_lc_rs` off. Structurally, `cargo tree -i
//! aws-lc-rs` must come back empty.)

#[test]
fn reqwest_client_builds_once_the_ring_provider_is_installed() {
    // Reproduces what the live binary does at every client build site. Without this,
    // the `.build()` below would panic: "No rustls crypto provider is configured."
    kaibo::tls::ensure_crypto_provider();

    // A no-default-provider rustls connector resolves its provider from the process
    // default during `build()`. This is the exact call consult.rs / image_gen.rs
    // make; it succeeds only because ring is wired in and installed above.
    reqwest::Client::builder()
        .build()
        .expect("reqwest client builds with the ring provider installed");

    // And the installed default really is a provider we put there (ring), not some
    // accidental fallback: a fresh ring provider must match the installed one.
    let installed = rustls::crypto::CryptoProvider::get_default()
        .expect("a process-default crypto provider is installed");
    let ring = rustls::crypto::ring::default_provider();
    assert_eq!(
        installed.cipher_suites.len(),
        ring.cipher_suites.len(),
        "the installed default provider should be ring's"
    );
}
