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
