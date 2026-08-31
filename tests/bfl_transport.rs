//! The BFL client driven over real sockets, offline — the transport truths the
//! pure-function tests in `src/bfl.rs` cannot see: the create call dials the named
//! op's own path with an `x-key` header, and the *poll* call dials the create
//! response's `polling_url` **verbatim** — a different host/port than the create
//! call's own base URL, proving it is never rebuilt from `base_url`. See the module
//! doc in `src/bfl.rs`'s "Polling is TLS-strict" section for the trust-shape
//! reasoning this file also exercises: `polling_url` is a provider-chosen address
//! carrying the `x-key` credential, so the poll leg runs over
//! `crate::tls::artifact_fetch_client` (https-only, no redirects) rather than the
//! general client `create` uses.
//!
//! Two one-shot servers stand in for the two hops: a "create" server answers the
//! POST, naming a "poll" server's address as its `polling_url`; if kaibo ever
//! reconstructed that URL from the create server's own base instead, the poll
//! server would never see a connection and the test would hang until the request
//! times out — the same negative-control shape `tests/gemini_images_transport.rs`
//! and `tests/dashscope_transport.rs` already rely on for their own "dials the
//! right place" assertions.
//!
//! The poll leg being TLS-strict means an offline plain-`TcpListener` can no longer
//! stand in for a *successful* poll exchange the way it used to — the same reason
//! `result.sample` (`cas::fetch_artifact_bytes`) never could. An offline test that
//! could reach a plaintext `polling_url` would be testing a kaibo that does not
//! exist, so this file proves the refusal instead: a plaintext `polling_url` is
//! refused with the poll listener seeing zero connections, and an `https://`
//! `polling_url` at a plain listener is dialled (proving the verbatim address) and
//! then fails its TLS handshake, since a bare `TcpListener` speaks no TLS. Every
//! poll outcome *other* than "refused before/at the wire" — `Pending`, `Ready`,
//! `Task not found`, and the rest — is covered by `src/bfl.rs`'s unit tests over
//! `parse_poll_response` and, for the inline-poll-then-defer decision specifically,
//! `inline_step`; a live exchange is `tests/bfl_live.rs`'s job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kaibo::bfl::{BflClient, BflImageModel};
use kaibo::media::{MediaModel, MediaRequest};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One captured HTTP request: the head as raw text, the body as JSON (or `Value::Null`
/// for a request with no body, e.g. every poll GET).
struct Captured {
    head: String,
    body: Value,
}

impl Captured {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or("")
    }

    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|line| {
            line.to_ascii_lowercase()
                .starts_with(&want)
                .then(|| line[want.len()..].trim().to_string())
        })
    }
}

/// Serve exactly one HTTP exchange (GET or POST) and hand back the captured request.
/// Returns the base URL to dial — an API root, since the client appends its own path.
async fn one_shot_server(
    status: u16,
    response_body: String,
) -> (String, tokio::sync::oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let head_end = loop {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client hung up mid-request");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        while buf.len() < head_end + content_length {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client hung up mid-body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = if content_length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap()
        };
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len(),
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
        tx.send(Captured { head, body }).ok();
    });
    (format!("http://{addr}"), rx)
}

fn request(prompt: &str) -> MediaRequest {
    MediaRequest {
        prompt: prompt.to_string(),
        ..Default::default()
    }
}

/// **The security-relevant case.** A plaintext `polling_url` — a provider-chosen
/// address that would carry the `x-key` credential — is refused with the poll
/// listener seeing ZERO connections: proof the refusal happens client-side, before
/// any byte of the poll request leaves, not merely that the eventual round trip goes
/// badly. Asserting on the returned error text alone would not rule out kaibo having
/// dialled the listener first; checking the listener's own accept count is what
/// does. The create server DOES see its POST, proving the chain reached the poll
/// step rather than failing earlier for an unrelated reason — a check that examines
/// nothing is the failure this test is written to avoid.
#[tokio::test]
async fn a_plaintext_polling_url_is_refused_with_the_poll_listener_seeing_no_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let poll_addr = listener.local_addr().unwrap();

    let polling_url = format!("http://{poll_addr}/v1/get_result?id=t-1");
    let (create_base, create_captured) = one_shot_server(
        200,
        serde_json::json!({"id": "t-1", "polling_url": polling_url}).to_string(),
    )
    .await;

    let client = BflClient::new("k", create_base, Duration::from_secs(2)).unwrap();
    let model = BflImageModel::from_parts(&client, "flux-dev");
    let err = model
        .generate(&request("a red cube"))
        .await
        .expect_err("a plaintext polling_url must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("https"), "names the requirement: {msg}");
    assert!(msg.contains(&polling_url), "names the offending URL: {msg}");

    // The listener must see zero connections — refused client-side, before any byte
    // of the request left, not discovered only after a real round trip failed.
    let accept_attempt = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
    assert!(
        accept_attempt.is_err(),
        "the poll listener must see zero connections — refused client-side"
    );

    // The chain reached the poll step: the create call happened and was answered,
    // so the refusal above is the poll leg's, not an earlier unrelated failure.
    let create = create_captured
        .await
        .expect("create server saw one request");
    assert_eq!(create.request_line(), "POST /v1/flux-dev HTTP/1.1");
}

/// The poll call dials the create response's `polling_url` **verbatim** — a
/// different host/port entirely than the create call's own base, which only a
/// literal (not reconstructed) address can reach. Proven by handing back an
/// `https://` URL at a bare `TcpListener`: the poll client dials it — the listener
/// observes a real TCP connection, which is only possible if the exact address was
/// used — and then fails its TLS handshake, since a bare listener speaks no TLS.
/// That failure is the expected shape here, not a test bug: this file cannot stand
/// up a real TLS endpoint, so proving the *dial* is the offline ceiling; a full
/// exchange is `tests/bfl_live.rs`'s job. The `x-key`-on-poll header assertion this
/// test used to make when the poll leg ran over plain HTTP is no longer observable
/// this way — TLS never completes, so no HTTP request is ever readable on the wire
/// — and now belongs to the live probe instead (an unauthenticated poll answers 401
/// there).
#[tokio::test]
async fn poll_dials_the_polling_url_verbatim_not_the_create_bases_host() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let poll_addr = listener.local_addr().unwrap();
    let connected = Arc::new(AtomicBool::new(false));
    let connected_flag = connected.clone();
    tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            connected_flag.store(true, Ordering::SeqCst);
        }
    });

    let polling_url = format!("https://{poll_addr}/v1/get_result?id=t-99");
    let (create_base, create_captured) = one_shot_server(
        200,
        serde_json::json!({
            "id": "t-99",
            "polling_url": polling_url,
            "cost": 0.05,
        })
        .to_string(),
    )
    .await;

    let client = BflClient::new("test-key-1", create_base, Duration::from_secs(5)).unwrap();
    let model = BflImageModel::from_parts(&client, "flux-2-pro");
    model
        .generate(&request("a red cube on a white table"))
        .await
        .expect_err("a bare TCP listener speaks no TLS, so the handshake must fail");

    // The connection attempt itself is the proof the poll dialled the exact
    // polling_url handed back by create, rather than reconstructing something
    // pointed at create_base — the accept only fires if the address was verbatim.
    assert!(
        connected.load(Ordering::SeqCst),
        "the poll listener must see a real connection attempt, proving the verbatim dial"
    );

    let create = create_captured
        .await
        .expect("create server saw one request");
    assert_eq!(
        create.request_line(),
        "POST /v1/flux-2-pro HTTP/1.1",
        "the client appends the op's own path to the configured base URL"
    );
    assert_eq!(create.header("x-key").as_deref(), Some("test-key-1"));
    assert_eq!(create.body["prompt"], "a red cube on a white table");
}

/// A 422 validation error on the create call renders readably, and no poll is ever
/// attempted — the create server is the only server started, so a second connection
/// would have nowhere to land.
#[tokio::test]
async fn a_422_on_create_renders_the_validation_detail_and_never_polls() {
    let (base, _captured) = one_shot_server(
        422,
        serde_json::json!({
            "detail": [{"loc": ["body", "width"], "msg": "Input should be a multiple of 32", "type": "value_error"}]
        })
        .to_string(),
    )
    .await;
    let client = BflClient::new("k", base, Duration::from_secs(5)).unwrap();
    let model = BflImageModel::from_parts(&client, "flux-dev");
    let err = model
        .generate(&request("a red cube"))
        .await
        .expect_err("a 422 must surface");
    let msg = format!("{err:#}");
    assert!(msg.contains("422"), "{msg}");
    assert!(msg.contains("body.width"), "{msg}");
    assert!(msg.contains("multiple of 32"), "{msg}");
}
