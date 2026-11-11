//! The per-role `effort` on a GPT 6 slot, from config to a real HTTP request.
//!
//! The links before this one are pinned elsewhere: the blob per model in
//! `src/consult/shaping.rs`, and the blob through rig's Responses body over a fake
//! transport in `tests/effort_wire.rs`. What only this file covers is the whole path a
//! call takes: a `[backends]` table with `wire = "responses"`, then `Arm::from_slot`,
//! then `oneshot`, then reqwest dialing `POST /v1/responses` on a real socket. Change
//! one file and check the other.
//!
//! The backend is aimed at a local socket that captures one request and answers 400.
//! The test reads the captured body and ignores the call's result.

use std::time::Duration;

use kaibo::config::{Config, ModelRole, ModelSlot};
use kaibo::consult::{known_efforts, oneshot, Arm, PhaseContext, ThinkingStyleOverride};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Captured {
    request_line: String,
    body: Value,
}

/// Serve one HTTP exchange, answer it with a 400, and hand back the request.
async fn capture_one() -> (String, tokio::sync::oneshot::Receiver<Captured>) {
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
        let body = serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap();
        // Hand the capture over before replying, so a client that already gave up
        // cannot lose a request that did arrive.
        let request_line = head.lines().next().unwrap_or_default().to_string();
        tx.send(Captured { request_line, body }).ok();
        let reply = r#"{"error":{"message":"captured","type":"invalid_request_error"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len(),
        );
        sock.write_all(response.as_bytes()).await.ok();
        sock.shutdown().await.ok();
    });
    (format!("http://{addr}/v1"), rx)
}

/// Build a hosted-shaped arm for `model` at `effort`, send one `oneshot`, and return
/// what reached the socket.
async fn sent_body(model: &str, effort: &str, style: Option<ThinkingStyleOverride>) -> Captured {
    let (base_url, rx) = capture_one().await;
    let cfg = Config::from_toml_str(&format!(
        r#"
        [backends.gpt]
        kind = "openai"
        base_url = "{base_url}"
        wire = "responses"
        "#
    ))
    .unwrap();
    let mut backend = cfg.resolve_backend("gpt").unwrap().clone();
    backend.request_timeout = Duration::from_secs(5);
    let slot = ModelSlot {
        effort: Some(effort.into()),
        thinking_style: style,
        ..ModelSlot::bare("gpt", model)
    };
    let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &cfg.defaults)
        .unwrap_or_else(|e| panic!("{model} at effort {effort:?} builds an arm: {e:#}"));
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        oneshot("ping", &[], &arm, &PhaseContext::default()),
    )
    .await
    .expect("the 400 returns promptly");
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("the request reached the socket")
        .expect("the capture task sent the request")
}

/// The GPT 6 ids OpenAI served on 2026-11-10 (`kaibo models --backend <openai>`). The
/// live probe `openai_effort_ceiling_is_per_model_live` in `tests/consult.rs` measures
/// the same five; keep the two lists together.
const GPT6_IDS: [&str; 5] = [
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-6-astra",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
];

#[tokio::test]
async fn every_effort_rung_reaches_the_responses_body_for_gpt_6() {
    for model in GPT6_IDS.into_iter().chain(["gpt-5.6-sol"]) {
        for effort in known_efforts() {
            let got = sent_body(model, effort, None).await;
            assert!(
                got.request_line.starts_with("POST /v1/responses "),
                "{model}: hosted GPT goes to the Responses endpoint, got {:?}",
                got.request_line
            );
            assert_eq!(got.body["model"], model);
            assert_eq!(
                got.body["reasoning"]["effort"], effort,
                "{model}: the slot effort {effort:?} reaches the wire unchanged: {}",
                got.body
            );
            for field in [
                "temperature",
                "top_p",
                "max_tokens",
                "max_completion_tokens",
            ] {
                assert!(
                    got.body.get(field).is_none(),
                    "{model} at {effort:?}: no `{field}` on a GPT reasoning model: {}",
                    got.body
                );
            }
            assert_eq!(
                got.body["max_output_tokens"], 16384,
                "{model} at {effort:?}: the default output budget rides as \
                 `max_output_tokens`: {}",
                got.body
            );
        }
    }
}

/// `thinking_style = "off"` sends no reasoning parameter at all, so the model runs at
/// OpenAI's default effort. This is the documented meaning of `off`; `effort = "none"`
/// is the setting that asks OpenAI to skip reasoning.
#[tokio::test]
async fn thinking_style_off_omits_reasoning_for_gpt_6() {
    for model in GPT6_IDS {
        let got = sent_body(model, "high", Some(ThinkingStyleOverride::Off)).await;
        // Without this, the test also passes on the chat wire, which sends no
        // `reasoning` either but does send sampling a GPT 6 model rejects.
        assert!(
            got.request_line.starts_with("POST /v1/responses "),
            "{model}: `off` stays on the Responses endpoint, got {:?}",
            got.request_line
        );
        assert!(
            got.body.get("reasoning").is_none(),
            "{model}: `off` sends no reasoning object: {}",
            got.body
        );
    }
}
