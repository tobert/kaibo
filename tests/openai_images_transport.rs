//! The openai-images client driven end to end over a real socket, offline — the
//! transport truths the pure-function tests cannot see and the paid live probe
//! should not be the only witness to: the URL the client actually dials, the
//! bearer header it sends, the JSON body as serialized on the wire, and the
//! response walking back into `Vec<MediaArtifact>`.
//!
//! Pattern precedent: `tests/llm_timeout.rs` stands up a local `TcpListener` to
//! exercise a real client with no network and no key. This file's listener goes
//! one step further: it reads the one request, hands back a canned response, and
//! surrenders the captured request for assertions.

use std::time::Duration;

use base64::Engine as _;
use kaibo::media::{FieldValue, MediaRequest};
use kaibo::openai_images::OpenAiImagesClient;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One captured HTTP request: the head (request line + headers, as raw text) and
/// the body parsed as JSON.
struct Captured {
    head: String,
    body: Value,
}

impl Captured {
    /// The request line, e.g. `POST /v1/images/generations HTTP/1.1`.
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or("")
    }

    /// A header's value, name matched case-insensitively.
    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|line| {
            line.to_ascii_lowercase()
                .starts_with(&want)
                .then(|| line[want.len()..].trim().to_string())
        })
    }
}

/// Serve exactly one HTTP exchange: accept a connection, read the request (head,
/// then `Content-Length` bytes of body), answer with `status` + a JSON `body`,
/// and hand the captured request back through the returned receiver. Returns the
/// base URL to dial (through `/v1`, the shape the client contract wants).
async fn one_shot_server(
    status: u16,
    response_body: String,
) -> (String, tokio::sync::oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Read until the end of the head, then exactly Content-Length body bytes.
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
    (format!("http://{addr}/v1"), rx)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The URL-defaulting family (dall-e-3), full round trip: the client POSTs to
/// exactly `{base}/images/generations`, authenticates with the bearer it was
/// built with, serializes the body with the caller's stated JSON types intact
/// (`n` an integer, `user` a string that LOOKS numeric, a bool typed), seeds
/// `response_format = "b64_json"` — and a multi-image response walks back into an
/// ordered `Vec<MediaArtifact>`.
#[tokio::test]
async fn dall_e_round_trip_sends_typed_body_and_collects_ordered_artifacts() {
    let response = serde_json::json!({
        "created": 1_722_000_000u64,
        "data": [
            { "b64_json": b64(b"first-image") },
            { "b64_json": b64(b"second-image") },
        ],
    });
    let (base_url, captured) = one_shot_server(200, response.to_string()).await;

    let client = OpenAiImagesClient::new("test-key-123", base_url, Duration::from_secs(5))
        .expect("client builds offline");
    let artifacts = client
        .generate(
            "dall-e-3",
            &MediaRequest {
                prompt: "two lighthouses".to_string(),
                fields: vec![
                    (
                        "n".to_string(),
                        FieldValue::Num(serde_json::Number::from(2)),
                    ),
                    ("user".to_string(), FieldValue::Str("123".to_string())),
                    ("transparent".to_string(), FieldValue::Bool(true)),
                    ("size".to_string(), FieldValue::Str("1024x1024".to_string())),
                ],
                inputs: Vec::new(),
            },
        )
        .await
        .expect("the canned 200 parses into artifacts");

    let req = captured.await.expect("the server captured one request");
    assert_eq!(
        req.request_line(),
        "POST /v1/images/generations HTTP/1.1",
        "the client appends its own route to the /v1 root"
    );
    assert_eq!(
        req.header("authorization").as_deref(),
        Some("Bearer test-key-123"),
        "the resolved key rides as a bearer"
    );
    assert_eq!(
        req.body.get("model").and_then(Value::as_str),
        Some("dall-e-3")
    );
    assert_eq!(
        req.body.get("prompt").and_then(Value::as_str),
        Some("two lighthouses")
    );
    assert_eq!(
        req.body.get("n"),
        Some(&Value::from(2)),
        "a caller number is a JSON integer on the wire"
    );
    assert_eq!(
        req.body.get("user"),
        Some(&Value::String("123".to_string())),
        "a numeric-looking caller string stays a string on the wire"
    );
    assert_eq!(req.body.get("transparent"), Some(&Value::Bool(true)));
    assert_eq!(
        req.body.get("response_format").and_then(Value::as_str),
        Some("b64_json"),
        "the URL-defaulting family gets the b64 contract stated"
    );

    assert_eq!(
        artifacts
            .iter()
            .map(|a| a.bytes.clone())
            .collect::<Vec<_>>(),
        vec![b"first-image".to_vec(), b"second-image".to_vec()],
        "both images land, in the provider's order"
    );
    assert!(artifacts.iter().all(|a| a.mime == "image/png"));
    assert!(artifacts.iter().all(|a| a.seed.is_none()));
}

/// The gpt-image family: `response_format` is ABSENT from the wire (the API
/// rejects the parameter; the family always returns b64), and a trailing slash on
/// the configured base URL still dials the exact same path.
#[tokio::test]
async fn gpt_image_omits_response_format_and_joins_a_trailing_slash_base() {
    let response = serde_json::json!({ "data": [{ "b64_json": b64(b"img") }] });
    let (base_url, captured) = one_shot_server(200, response.to_string()).await;

    let client = OpenAiImagesClient::new(
        "k",
        format!("{base_url}/"), // trailing slash: joining must not double it
        Duration::from_secs(5),
    )
    .expect("client builds offline");
    client
        .generate(
            "gpt-image-1",
            &MediaRequest {
                prompt: "p".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("the canned 200 parses");

    let req = captured.await.expect("captured");
    assert_eq!(
        req.request_line(),
        "POST /v1/images/generations HTTP/1.1",
        "a trailing-slash base joins to the same exact path"
    );
    assert!(
        req.body.get("response_format").is_none(),
        "gpt-image never carries response_format, got {}",
        req.body
    );
}

/// A provider error over the real transport surfaces as the rendered provider
/// error, not a parse panic — the wire half of `parse_response`'s non-2xx arm.
#[tokio::test]
async fn provider_401_surfaces_readably_over_the_wire() {
    let response = serde_json::json!({
        "error": { "message": "Incorrect API key provided", "type": "invalid_request_error" }
    });
    let (base_url, _captured) = one_shot_server(401, response.to_string()).await;

    let client = OpenAiImagesClient::new("bad-key", base_url, Duration::from_secs(5)).unwrap();
    let err = client
        .generate(
            "gpt-image-1",
            &MediaRequest {
                prompt: "p".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect_err("a 401 is a provider error");
    let msg = err.to_string();
    assert!(
        msg.contains("401") && msg.contains("Incorrect API key"),
        "the status and the provider's message both surface, got: {msg}"
    );
}
