//! TLS crypto-provider wiring.
//!
//! We build reqwest with the `rustls-no-provider` feature (see Cargo.toml): rustls
//! is compiled in, but no crypto provider is baked as the process default. reqwest
//! then reads the *process-default* [`rustls::crypto::CryptoProvider`] when it
//! builds a client and **panics loudly** if none is installed — it never silently
//! falls back to plaintext. So exactly one thing must happen before the first HTTPS
//! client is built: install a provider.
//!
//! We install **ring** rather than aws-lc-rs. ring needs no C toolchain or cmake,
//! so `x86_64-unknown-linux-musl` and the other static targets link cleanly with no
//! system dependencies — the point of the whole TLS arrangement. The choice is
//! pinned at compile time too: this module names `rustls::crypto::ring`, which only
//! exists because Cargo.toml enables rustls's `ring` feature (and not `aws_lc_rs`).

use std::sync::Once;
use std::time::Duration;

use anyhow::{anyhow, Result};

/// Install ring as the process-wide default rustls [`CryptoProvider`], exactly once.
///
/// Idempotent and cheap after the first call (guarded by a [`Once`]), so every
/// real reqwest-client construction site calls it unconditionally. That way no
/// entry point — the `kaibo` binary, an integration test, a future embedder of the
/// library — can forget to install a provider and hit reqwest's no-provider panic.
///
/// Panics only if installation genuinely fails (another provider was already
/// installed by someone outside this `Once`), which is an operator/wiring error we
/// want surfaced immediately, not papered over.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("install ring as the default rustls crypto provider");
    });
}

/// Build a reqwest HTTPS client carrying a per-request deadline — the **one**
/// construction site for every provider client (rig completions and provider batches
/// alike), so the `rustls-no-provider` + ring contract lives in exactly one place
/// instead of being hand-rolled at each call site. Installs the ring provider first
/// (idempotent; reqwest panics on a missing default rather than falling back to
/// plaintext), then builds off the process-default rustls provider — no `native-tls`,
/// no OpenSSL, no C.
///
/// The client is bounded because rig exposes no native timeout and its prompt loop is
/// non-streaming: a backend that connects but never answers would otherwise hang the
/// whole call with no brake (the 2026-06-06 ~29-min wedge; `docs/issues.md`).
/// `timeout` caps a single completion; `connect_timeout` fails a dead endpoint fast,
/// capped at the deadline so a sub-10s backend timeout still dominates.
pub fn https_client(request_timeout: Duration) -> Result<reqwest::Client> {
    ensure_crypto_provider();
    reqwest::Client::builder()
        .timeout(request_timeout)
        .connect_timeout(request_timeout.min(Duration::from_secs(10)))
        .build()
        .map_err(|e| anyhow!("http client init: {e}"))
}
