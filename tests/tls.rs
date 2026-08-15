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
    // default during `build()`. This is the raw call under `tls::https_client`, the one
    // build site consult + batch route through; it succeeds only because ring is wired in
    // and installed above.
    reqwest::Client::builder()
        .build()
        .expect("reqwest client builds with the ring provider installed");

    // The shared helper itself builds — it installs ring internally, so this holds even
    // as the *sole* provider clients' build path. Guards the helper directly, not just the
    // raw builder above.
    kaibo::tls::https_client(std::time::Duration::from_secs(5))
        .expect("tls::https_client builds a real reqwest client");

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

/// The artifact-fetch client refuses plaintext on its own, not just because
/// `cas::fetch_artifact_bytes` checks the scheme up front.
///
/// This is the half that check cannot cover: reqwest follows redirects, and its
/// `https_only` flag is off by default, so an `https` artifact link that bounced to
/// `http` would be followed. Proving the CLIENT refuses `http` proves the property
/// holds for every hop, redirect targets included.
///
/// Written as a **differential**, because the obvious version of this test passes for
/// the wrong reason: a listener that accepts and hangs up makes *both* clients error,
/// so the test stays green even with `https_only` removed. Here the server answers a
/// real `200`, and the assertion is that the ordinary client GETS it while the fetch
/// client does not — which can only be true if the scheme restriction is doing the
/// work. Verified by removing `https_only(true)` and watching this fail.
///
/// Coverage limit, stated rather than papered over: the fetch client's OTHER setting,
/// `Policy::none()`, has no offline test. `https_only` means it cannot be pointed at
/// a local plaintext fixture, and serving TLS here would need a certificate this
/// suite has no way to trust. The redirect path is exercised only by the `#[ignore]`d
/// live probe. An assertion that merely constructed both clients was written and
/// deleted — it could not fail, and a test that cannot fail is worse than none.
#[tokio::test]
async fn the_artifact_fetch_client_refuses_plaintext_the_ordinary_client_accepts() {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Answer every comer with a complete, valid response.
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let body = "ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    let url = format!("http://{addr}/artifact.png");

    // The control FIRST: if this fails, the fixture is broken and the refusal below
    // would prove nothing.
    let ordinary = kaibo::tls::https_client(std::time::Duration::from_secs(5))
        .expect("the ordinary client builds");
    let ok = ordinary
        .get(&url)
        .send()
        .await
        .expect("the ordinary client has no scheme restriction, so plaintext works");
    assert_eq!(
        ok.status().as_u16(),
        200,
        "the fixture server must really answer, or the refusal below means nothing"
    );

    let fetch = kaibo::tls::artifact_fetch_client(std::time::Duration::from_secs(5))
        .expect("the fetch client builds");
    let err = fetch
        .get(&url)
        .send()
        .await
        .expect_err("the fetch client must refuse plaintext even when it would work");
    assert!(
        !err.is_timeout(),
        "the refusal must be a scheme refusal, not a timeout: {err}"
    );
}

/// kaibo names itself on the wire. Observed on a real socket rather than asserted
/// against the builder, because the question is what a provider actually receives.
///
/// This started as a gap, not a regression: reqwest sends no `User-Agent` unless one
/// is set, so kaibo's traffic arrived carrying only `accept` and `host`. The
/// assertions below are deliberately about the header's PRESENCE and shape, not its
/// exact version — pinning the version would turn every release into a test edit.
#[tokio::test]
async fn outbound_requests_name_kaibo_and_its_version() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Serve one request and hand back its head.
    async fn capture(port_tx: tokio::sync::oneshot::Sender<String>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        port_tx
            .send(listener.local_addr().unwrap().to_string())
            .unwrap();
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap();
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await;
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(capture(tx));
    let addr = rx.await.unwrap();
    let client = kaibo::tls::https_client(std::time::Duration::from_secs(5)).unwrap();
    let _ = client.get(format!("http://{addr}/probe")).send().await;
    let head = server.await.unwrap();

    let ua = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("user-agent:"))
        .map(|l| l["user-agent:".len()..].trim().to_string())
        .unwrap_or_else(|| panic!("no User-Agent reached the server; head was:\n{head}"));
    assert_eq!(
        ua,
        kaibo::tls::USER_AGENT,
        "the wire value must be exactly the constant, not a reqwest default"
    );
    assert!(
        ua.starts_with("kaibo/"),
        "a provider reading its logs should see kaibo by name, got {ua:?}"
    );
    assert!(
        ua.len() > "kaibo/".len(),
        "the version must actually be interpolated, got {ua:?}"
    );
    // Short form, decided deliberately: no URL, no OS, no arch.
    assert!(
        !ua.contains("http") && !ua.contains(' '),
        "the short form carries a name and a version and nothing else, got {ua:?}"
    );
}
