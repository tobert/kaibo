//! The Decisions client driven over a real socket, offline: the request carries the
//! bearer key and the probed body shape, a provider refusal reaches the caller with the
//! provider's own message, and a reply that does not answer the questions asked is an
//! error rather than a repaired value. The wire shapes come from the live probe of
//! 2026-10-06 (see the module doc in `src/decide.rs`).

use std::collections::BTreeMap;
use std::time::Duration;

use kaibo::decide::{
    Answer, Criteria, DecideError, DecisionClient, DecisionRequest, Question, QuestionKind,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Captured {
    head: String,
    body: Value,
}

/// Serve one HTTP exchange and hand back the captured request.
async fn one_shot_server(
    status: u16,
    response_body: &'static str,
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
        let body = serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len(),
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
        tx.send(Captured { head, body }).ok();
    });
    (format!("http://{addr}/api/alpha/decisions"), rx)
}

fn denied_request() -> DecisionRequest {
    request().deny_data_collection()
}

fn request() -> DecisionRequest {
    let q = BTreeMap::from([
        (
            "verdict".to_string(),
            Question {
                kind: QuestionKind::Choice,
                instructions: "how risky".into(),
                criteria: Some(Criteria::Options(BTreeMap::from([
                    ("safe".to_string(), "read-only".to_string()),
                    ("risky".to_string(), "changes something".to_string()),
                ]))),
            },
        ),
        (
            "pushes".to_string(),
            Question {
                kind: QuestionKind::Noul,
                instructions: "it publishes".into(),
                criteria: None,
            },
        ),
    ]);
    DecisionRequest::new("typesafe/jev-1.13", "git push".into(), q).unwrap()
}

fn client(url: String) -> DecisionClient {
    DecisionClient::new(url, "test-key", Duration::from_secs(5)).unwrap()
}

const OK_BODY: &str = r#"{"model":"typesafe/jev-1.13-20260917","answers":{
  "verdict":{"type":"choice","choice":"risky","probabilities":{"safe":0.01,"risky":0.99},"confidence":0.99},
  "pushes":{"type":"noul","noul":0.96}},
  "usage":{"input_tokens":120,"output_tokens":30,"cost":0.00001},"id":"gen-dec-1"}"#;

#[tokio::test]
async fn the_request_carries_the_key_and_the_probed_body() {
    let (url, captured) = one_shot_server(200, OK_BODY).await;
    let resp = client(url).decide(&request()).await.unwrap();
    let cap = captured.await.unwrap();

    assert!(
        cap.head.starts_with("POST /api/alpha/decisions "),
        "{}",
        cap.head
    );
    assert!(
        cap.head
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key"),
        "the bearer key must be sent: {}",
        cap.head
    );
    assert_eq!(cap.body["model"], "typesafe/jev-1.13");
    assert_eq!(cap.body["state"], "git push");
    assert_eq!(cap.body["questions"]["verdict"]["type"], "choice");
    assert_eq!(
        cap.body["questions"]["verdict"]["criteria"]["risky"],
        "changes something"
    );
    assert_eq!(cap.body["questions"]["pushes"]["type"], "noul");

    assert_eq!(resp.model, "typesafe/jev-1.13-20260917");
    assert_eq!(resp.usage.cost, Some(0.00001));
    assert!(matches!(resp.answers["pushes"], Answer::Noul { noul } if noul == 0.96));
}

#[tokio::test]
async fn a_provider_refusal_carries_the_providers_message_and_status() {
    let (url, _cap) = one_shot_server(
        400,
        r#"{"error":{"message":"Model typesafe/nope does not exist","code":400}}"#,
    )
    .await;
    let err = client(url).decide(&request()).await.unwrap_err();
    match err {
        DecideError::Provider { status, message } => {
            assert_eq!(status, 400);
            assert_eq!(message, "Model typesafe/nope does not exist");
        }
        other => panic!("expected a Provider error, got {other}"),
    }
}

#[tokio::test]
async fn a_reply_that_skips_a_question_is_an_error_not_a_default() {
    let (url, _cap) = one_shot_server(
        200,
        r#"{"model":"m","answers":{"pushes":{"type":"noul","noul":0.5}},"usage":{}}"#,
    )
    .await;
    let err = client(url).decide(&request()).await.unwrap_err();
    assert!(
        matches!(&err, DecideError::Mismatch { id, .. } if id == "verdict"),
        "{err}"
    );
}

#[tokio::test]
async fn an_unreadable_reply_is_named_as_unreadable() {
    let (url, _cap) = one_shot_server(200, r#"{"not":"a decision"}"#).await;
    let err = client(url).decide(&request()).await.unwrap_err();
    assert!(matches!(err, DecideError::Unreadable(_)), "{err}");
}

#[tokio::test]
async fn a_data_collection_pin_reaches_the_wire() {
    let (url, captured) = one_shot_server(200, OK_BODY).await;
    client(url).decide(&denied_request()).await.unwrap();
    let cap = captured.await.unwrap();
    assert_eq!(cap.body["provider"]["data_collection"], "deny");
}

#[tokio::test]
async fn no_pin_means_no_provider_field() {
    let (url, captured) = one_shot_server(200, OK_BODY).await;
    client(url).decide(&request()).await.unwrap();
    let cap = captured.await.unwrap();
    assert!(cap.body.get("provider").is_none(), "{}", cap.body);
}
