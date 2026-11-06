//! The web client driven over a real socket, offline: a search and a fetch carry the
//! bearer key and DeepSeek's documented body shapes, a retryable status is retried, a
//! non-retryable one reaches the caller with the provider's own message, and an empty
//! result list is a success. Wire shapes are from DeepSeek's reference (see the module
//! doc in `src/web/mod.rs`).

use std::time::Duration;

use kaibo::web::{SearchRequest, WebClient, WebError};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Captured {
    head: String,
    body: Value,
}

/// Serve one HTTP exchange per `(status, body)` in order and hand back each captured
/// request.
async fn server(
    replies: Vec<(u16, &'static str)>,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, response_body) in replies {
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
            let body = serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap();
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            tx.send(Captured { head, body }).ok();
        }
    });
    (format!("http://{addr}"), rx)
}

fn client(base: &str) -> WebClient {
    WebClient::new(
        format!("{base}/v1/tools/search"),
        format!("{base}/v1/tools/fetch"),
        "sk-web-test",
        Duration::from_secs(10),
    )
    .unwrap()
}

const RESULTS: &str = r#"{"results":[{"rerank_score":2.1,"title":"turso 0.8 notes",
"url":"https://example.com/turso","snippet":"short","summary":"the long summary",
"content":"","site_name":"Example","site_icon":null,"date":"2026-10-01T00:00:00+00:00",
"authority":3}]}"#;

#[tokio::test]
async fn a_search_sends_the_documented_body_and_reads_results() {
    let (base, mut rx) = server(vec![(200, RESULTS)]).await;
    let req = SearchRequest::new("turso MP-WAL", 5, Some("is MP-WAL safe?")).unwrap();
    let results = client(&base).search(&req).await.unwrap();

    let got = rx.recv().await.unwrap();
    assert!(
        got.head.starts_with("POST /v1/tools/search "),
        "{}",
        got.head
    );
    assert!(
        got.head
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-web-test"),
        "{}",
        got.head
    );
    assert_eq!(
        got.body,
        serde_json::json!({"query": "turso MP-WAL", "count": 5, "ori_question": "is MP-WAL safe?"})
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].url, "https://example.com/turso");
    assert_eq!(results[0].summary, "the long summary");
    assert_eq!(results[0].authority, Some(3));
}

#[tokio::test]
async fn an_empty_result_list_is_a_success() {
    let (base, _rx) = server(vec![(200, r#"{"results":[]}"#)]).await;
    let req = SearchRequest::new("nothing matches this", 5, None).unwrap();
    assert!(client(&base).search(&req).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_fetch_sends_the_url_and_reads_markdown() {
    let (base, mut rx) = server(vec![(
        200,
        r##"{"title":"Docs","content":"# Docs\nbody"}"##,
    )])
    .await;
    let page = client(&base)
        .fetch("https://example.com/docs")
        .await
        .unwrap();
    let got = rx.recv().await.unwrap();
    assert!(
        got.head.starts_with("POST /v1/tools/fetch "),
        "{}",
        got.head
    );
    assert_eq!(
        got.body,
        serde_json::json!({"url": "https://example.com/docs"})
    );
    assert_eq!(page.title, "Docs");
    assert_eq!(page.content, "# Docs\nbody");
}

#[tokio::test]
async fn a_rate_limit_is_retried() {
    let (base, mut rx) = server(vec![
        (
            429,
            r#"{"error":{"message":"slow down","type":"x","param":null,"code":null}}"#,
        ),
        (200, RESULTS),
    ])
    .await;
    let req = SearchRequest::new("q", 5, None).unwrap();
    let results = client(&base).search(&req).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(
        rx.recv().await.is_some() && rx.recv().await.is_some(),
        "two requests made"
    );
}

#[tokio::test]
async fn a_404_reaches_the_caller_with_the_provider_message_and_no_retry() {
    let (base, mut rx) = server(vec![(
        404,
        r#"{"error":{"message":"no usable content","type":"x","param":null,"code":null}}"#,
    )])
    .await;
    let err = client(&base)
        .fetch("https://example.com/gone")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WebError::Provider {
            status: 404,
            message: "no usable content".into()
        }
    );
    let text = err.to_string();
    assert!(text.contains("Choose a different source"), "{text}");
    rx.recv().await.unwrap();
    // One request only: a 404 is not retried. The server task served its one reply and
    // exited, so the channel closes with nothing more.
    assert!(rx.recv().await.is_none());
}

/// The client carries the operator's key, so a redirect is never followed: a 302 reaches
/// the caller as its status, and the second socket (where the redirect points) sees no
/// request. With reqwest's default policy this would follow the hop and fail differently.
#[tokio::test]
async fn a_redirect_is_reported_not_followed() {
    let (target, mut target_rx) = server(vec![(200, RESULTS)]).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let location = format!("{target}/v1/tools/search");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
    });
    let base = format!("http://{addr}");
    let req = SearchRequest::new("q", 5, None).unwrap();
    let err = client(&base).search(&req).await.unwrap_err();
    assert!(
        matches!(err, WebError::Provider { status: 302, .. }),
        "{err:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), target_rx.recv())
            .await
            .is_err(),
        "the redirect target must see no request"
    );
}
