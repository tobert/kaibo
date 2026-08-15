//! The DashScope client driven over a real socket, offline — the transport truths
//! the pure-function tests cannot see and the paid live probe should not be the only
//! witness to: the URL the client dials, the bearer it sends, the JSON body as
//! serialized on the wire, and a provider error rendering readably.
//!
//! Sibling of `tests/openai_images_transport.rs`, with one deliberate difference.
//! DashScope answers with artifact *links*, and `cas::fetch_artifact_bytes` takes
//! `https` only — so a plain `TcpListener` cannot serve the second hop. This file
//! therefore covers the generation exchange in full and pins that the fetch guard
//! fires on a non-TLS link, naming the URL. The walk from fetched bytes into a
//! `MediaArtifact` is covered by `artifact_mime`'s unit tests in `src/dashscope.rs`
//! and end to end by the `#[ignore]`d live probe in `tests/dashscope_live.rs`.
//!
//! Keeping the scheme check un-mocked is the point: an offline test that could reach
//! a plaintext artifact URL would be testing a kaibo that does not exist.

use std::time::Duration;

use kaibo::dashscope::{DashScopeClient, DashScopeImageModel};
use kaibo::media::{FieldValue, MediaModel, MediaRequest};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One captured HTTP request: the head as raw text, the body as JSON.
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

/// Serve exactly one HTTP exchange and hand back the captured request. Returns the
/// base URL to dial — an API *root*, since this client appends its own route.
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
            .expect("a JSON POST always declares Content-Length");
        while buf.len() < head_end + content_length {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client hung up mid-body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let body: Value =
            serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap();
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

/// A generation response carrying `n` images as links.
fn image_response(urls: &[&str]) -> String {
    let choices: Vec<Value> = urls
        .iter()
        .map(|u| serde_json::json!({"message": {"role": "assistant", "content": [{"image": u, "type": "image"}]}}))
        .collect();
    serde_json::json!({
        "request_id": "req-1",
        "output": {"choices": choices},
        "usage": {"image_count": urls.len(), "size": "1280*1280"},
    })
    .to_string()
}

/// The full generation exchange: the client POSTs to exactly
/// `{base}/api/v1/services/aigc/multimodal-generation/generation`, authenticates
/// with its bearer, nests the prompt in DashScope's message envelope, seeds
/// `enable_interleave`, and preserves each caller field's stated JSON type — `n` an
/// integer the provider type-checks, `negative_prompt` a string.
#[tokio::test]
async fn generation_dials_the_route_and_sends_a_typed_body() {
    let (base_url, captured) =
        one_shot_server(200, image_response(&["http://artifact.test/1.png"])).await;

    let client = DashScopeClient::new("test-key-123".to_string(), base_url, Duration::from_secs(5))
        .expect("client builds");
    let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");
    let request = MediaRequest {
        prompt: "a red cube on a white table".to_string(),
        fields: vec![
            (
                "n".to_string(),
                FieldValue::Num(serde_json::Number::from(2)),
            ),
            (
                "negative_prompt".to_string(),
                FieldValue::Str("blurry".to_string()),
            ),
        ],
        ..Default::default()
    };
    // The generation half succeeds; the artifact fetch is what fails, and the next
    // test pins that. Here the request is the subject.
    let _ = model.generate(&request).await;

    let captured = captured.await.expect("the server captured one request");
    assert_eq!(
        captured.request_line(),
        "POST /api/v1/services/aigc/multimodal-generation/generation HTTP/1.1",
        "the client appends its own route to the configured base URL"
    );
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer test-key-123")
    );
    assert_eq!(
        captured.body["model"],
        serde_json::json!("wan2.6-t2i"),
        "the slot's model id rides the body, not the URL"
    );
    assert_eq!(
        captured.body["input"]["messages"][0]["content"][0]["text"],
        serde_json::json!("a red cube on a white table")
    );
    assert_eq!(
        captured.body["parameters"]["enable_interleave"],
        serde_json::json!(true),
        "a text-only prompt is refused without this, so kaibo seeds it"
    );
    assert_eq!(captured.body["parameters"]["n"], serde_json::json!(2));
    assert!(
        captured.body["parameters"]["n"].is_i64(),
        "n must reach the wire as an integer, not a float or a string"
    );
    assert_eq!(
        captured.body["parameters"]["negative_prompt"],
        serde_json::json!("blurry")
    );
}

/// A plaintext artifact link is refused, and the refusal names the URL and the
/// requirement. This is the guard that keeps kaibo's one outbound artifact fetch on
/// TLS; a test that let it through would be testing a different program.
#[tokio::test]
async fn a_plaintext_artifact_link_is_refused_naming_the_url() {
    let (base_url, _captured) =
        one_shot_server(200, image_response(&["http://artifact.test/1.png"])).await;

    let client = DashScopeClient::new("k".to_string(), base_url, Duration::from_secs(5))
        .expect("client builds");
    let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");
    let err = model
        .generate(&MediaRequest {
            prompt: "a red cube".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("an http artifact link must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("https") && msg.contains("http://artifact.test/1.png"),
        "the refusal names the requirement and the offending URL, got: {msg}"
    );
    assert!(
        msg.contains("artifact 0"),
        "the refusal names which artifact failed, got: {msg}"
    );
}

/// A provider error renders as `code: message` rather than a raw JSON blob, and the
/// status rides along — the difference between "fix the key" and "fix the request".
#[tokio::test]
async fn a_provider_error_renders_readably_with_its_status() {
    let body = serde_json::json!({
        "request_id": "req-2",
        "code": "InvalidApiKey",
        "message": "Invalid API-key provided.",
    })
    .to_string();
    let (base_url, _captured) = one_shot_server(401, body).await;

    let client = DashScopeClient::new("bad".to_string(), base_url, Duration::from_secs(5))
        .expect("client builds");
    let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");
    let err = model
        .generate(&MediaRequest {
            prompt: "a red cube".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("a 401 must surface");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("401") && msg.contains("InvalidApiKey") && msg.contains("Invalid API-key"),
        "the error names the status, the provider code, and its message, got: {msg}"
    );
}

/// A 2xx carrying no image is refused rather than handed on as an empty artifact
/// list — zero artifacts is never a successful generation.
#[tokio::test]
async fn a_response_with_no_images_is_refused() {
    let body = serde_json::json!({
        "request_id": "req-3",
        "output": {"choices": [{"message": {"content": [{"text": "no picture"}]}}]},
    })
    .to_string();
    let (base_url, _captured) = one_shot_server(200, body).await;

    let client = DashScopeClient::new("k".to_string(), base_url, Duration::from_secs(5))
        .expect("client builds");
    let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");
    let err = model
        .generate(&MediaRequest {
            prompt: "a red cube".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("no images is not a success");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("without an image") && msg.contains("zero artifacts"),
        "the refusal says what came back and why it is not a success, got: {msg}"
    );
    assert!(
        msg.contains("again"),
        "and it says what to do next, got: {msg}"
    );
}
