//! The BFL client driven over real sockets, offline — the transport truths the
//! pure-function tests in `src/bfl.rs` cannot see: the create call dials the named
//! op's own path with an `x-key` header, and the *poll* call dials the create
//! response's `polling_url` **verbatim** — a different host/port than the create
//! call's own base URL, proving it is never rebuilt from `base_url`. See the module
//! doc in `src/bfl.rs` for why that distinction matters (a regional polling host).
//!
//! Two one-shot servers stand in for the two hops: a "create" server answers the
//! POST, naming a "poll" server's address as its `polling_url`; if kaibo ever
//! reconstructed that URL from the create server's own base instead, the poll
//! server would never see a connection and the test would hang until the request
//! times out — the same negative-control shape `tests/gemini_images_transport.rs`
//! and `tests/dashscope_transport.rs` already rely on for their own "dials the
//! right place" assertions.
//!
//! The final hop — fetching `result.sample` — takes `https` only
//! (`cas::fetch_artifact_bytes`), so a plain `TcpListener` cannot serve it; that walk
//! is covered by `artifact_mime`'s unit tests in `src/bfl.rs` and, end to end, by the
//! `#[ignore]`d live probe in `tests/bfl_live.rs`. Keeping the scheme check
//! un-mocked here is the point, same as the DashScope precedent: an offline test
//! that could reach a plaintext artifact URL would be testing a kaibo that does not
//! exist.

use std::time::Duration;

use kaibo::bfl::{BflClient, BflImageModel};
use kaibo::media::{MediaModel, MediaOutcome, MediaRequest};
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

/// The create call dials `{base}{op.path}` with `x-key`, and the poll call dials the
/// create response's `polling_url` **verbatim** — a different host entirely, which
/// only a literal (not reconstructed) URL can reach. The poll server answers `Ready`
/// with a plaintext `sample` link, which the fetch guard then refuses — proving the
/// whole create → poll → fetch chain ran, with TLS enforcement never bypassed offline.
#[tokio::test]
async fn poll_dials_the_polling_url_verbatim_not_the_create_bases_host() {
    let (poll_base, poll_captured) = one_shot_server(
        200,
        serde_json::json!({
            "id": "t-99",
            "status": "Ready",
            "result": {"sample": "http://artifact.test/0.png"},
        })
        .to_string(),
    )
    .await;
    let polling_url = format!("{poll_base}/v1/get_result?id=t-99");

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
    let err = model
        .generate(&request("a red cube on a white table"))
        .await
        .expect_err("the sample link is plaintext, so the fetch guard refuses it");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("https") && msg.contains("http://artifact.test/0.png"),
        "the refusal names the requirement and the offending URL, got: {msg}"
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

    let poll = poll_captured.await.expect("poll server saw one request");
    assert!(
        poll.request_line()
            .starts_with("GET /v1/get_result?id=t-99 "),
        "the poll dialled the create response's polling_url verbatim, got: {}",
        poll.request_line()
    );
    assert_eq!(
        poll.header("x-key").as_deref(),
        Some("test-key-1"),
        "the poll call authenticates too"
    );
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

/// `Task not found` on the poll surfaces its own clear error, distinct from a
/// generic provider error, over the real wire — not just the pure-function tests.
#[tokio::test]
async fn task_not_found_on_poll_is_reported_over_the_wire() {
    let (poll_base, _poll_captured) = one_shot_server(
        200,
        serde_json::json!({"id": "t-1", "status": "Task not found"}).to_string(),
    )
    .await;
    let polling_url = format!("{poll_base}/v1/get_result?id=t-1");
    let (create_base, _create_captured) = one_shot_server(
        200,
        serde_json::json!({"id": "t-1", "polling_url": polling_url}).to_string(),
    )
    .await;
    let client = BflClient::new("k", create_base, Duration::from_secs(5)).unwrap();
    let model = BflImageModel::from_parts(&client, "flux-dev");
    let err = model
        .generate(&request("a red cube"))
        .await
        .expect_err("an expired task id must surface");
    assert!(format!("{err:#}").contains("Generate again"));
}

/// When the provider is still working past the inline-poll budget, `generate`
/// returns `Deferred` carrying the same `polling_url` the create call handed
/// back — the fallback shape the background job lane (or the CLI's own wait loop)
/// continues from via a single `poll` GET. The budget is forced to zero so this
/// resolves after exactly one poll, not real wall-clock time.
#[tokio::test]
async fn a_still_pending_generation_defers_with_the_same_polling_url() {
    let (poll_base, _poll_captured) = one_shot_server(
        200,
        serde_json::json!({"id": "t-7", "status": "Pending"}).to_string(),
    )
    .await;
    let polling_url = format!("{poll_base}/v1/get_result?id=t-7");
    let (create_base, _create_captured) = one_shot_server(
        200,
        serde_json::json!({"id": "t-7", "polling_url": polling_url}).to_string(),
    )
    .await;
    let client = BflClient::new("k", create_base, Duration::from_secs(5)).unwrap();
    let model = BflImageModel::from_parts(&client, "flux-dev")
        .with_inline_poll_timing(Duration::ZERO, Duration::from_millis(1));
    let outcome = model
        .generate(&request("a red cube"))
        .await
        .expect("a pending generation is not an error");
    let MediaOutcome::Deferred(job) = outcome else {
        panic!("expected Deferred, got {outcome:?}");
    };
    assert_eq!(job.0, polling_url, "the same polling_url rides the job id");
}
