//! The gemini-images client over a real socket, offline.
//!
//! The transport truths the pure-function tests cannot see: the URL actually dialled
//! (Gemini puts the model *in the path* with a `:generateContent` suffix, which is
//! unlike every other backend kaibo speaks), and the credential riding a header rather
//! than the `?key=` query parameter Google also accepts — a header keeps the key out of
//! anything that logs a URL.
//!
//! Pattern precedent: `tests/openai_images_transport.rs`. Teeth: change the path or move
//! the key to a query parameter and these fail.

use std::time::Duration;

use base64::Engine as _;
use kaibo::gemini_images::GeminiImagesClient;
use kaibo::media::{MediaOutcome, MediaRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Captured {
    head: String,
    body: serde_json::Value,
}

impl Captured {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or("")
    }
    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with(&want)
                .then(|| l[want.len()..].trim().to_string())
        })
    }
}

async fn one_shot(
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
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .expect("a JSON POST declares Content-Length");
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client hung up mid-body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
        tx.send(Captured { head, body }).ok();
    });
    (format!("http://{addr}"), rx)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The model rides in the path with a `:generateContent` suffix, the key rides a header,
/// and an interleaved text-plus-image response walks back into artifacts plus the note.
#[tokio::test]
async fn the_model_is_in_the_path_and_the_key_is_in_a_header() {
    let response = serde_json::json!({
        "candidates": [{"content": {"parts": [
            {"text": "Here it is, with the sign moved left."},
            {"inlineData": {"mimeType": "image/png", "data": b64(b"the-bytes")}}
        ]}}]
    })
    .to_string();
    let (base, rx) = one_shot(200, response).await;
    let client = GeminiImagesClient::new("k-secret", &base, Duration::from_secs(5)).unwrap();

    let outcome = client
        .generate(
            "gemini-3-flash-image",
            &MediaRequest {
                prompt: "a lighthouse".into(),
                fields: Vec::new(),
                inputs: Vec::new(),
                op: None,
            },
        )
        .await
        .expect("round trips");

    let MediaOutcome::Complete { artifacts, note } = outcome else {
        panic!("generateContent is synchronous")
    };
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].bytes, b"the-bytes");
    assert_eq!(
        note.as_deref(),
        Some("Here it is, with the sign moved left.")
    );

    let cap = rx.await.unwrap();
    assert_eq!(
        cap.request_line(),
        "POST /v1beta/models/gemini-3-flash-image:generateContent HTTP/1.1",
        "the model is a path segment here, not a body field"
    );
    assert_eq!(cap.header("x-goog-api-key").as_deref(), Some("k-secret"));
    assert!(
        !cap.request_line().contains("key="),
        "the credential must not reach the URL, where it would be logged"
    );
    assert_eq!(cap.body["contents"][0]["parts"][0]["text"], "a lighthouse");
    assert_eq!(
        cap.body["generationConfig"]["responseModalities"],
        serde_json::json!(["TEXT", "IMAGE"])
    );
}

/// A 200 carrying only text is the refusal shape, and the client turns it into an error
/// that repeats what the model said — over the wire, not just in the parser.
#[tokio::test]
async fn a_text_only_answer_becomes_an_error_carrying_the_models_words() {
    let response = serde_json::json!({
        "candidates": [{"content": {"parts": [{"text": "I can't do that."}]}}]
    })
    .to_string();
    let (base, _rx) = one_shot(200, response).await;
    let client = GeminiImagesClient::new("k", &base, Duration::from_secs(5)).unwrap();
    let err = client
        .generate(
            "gemini-3-flash-image",
            &MediaRequest {
                prompt: "something refused".into(),
                fields: Vec::new(),
                inputs: Vec::new(),
                op: None,
            },
        )
        .await
        .expect_err("no image came back");
    assert!(err.to_string().contains("I can't do that."), "{err}");
}
