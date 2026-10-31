//! The real transport, end to end and offline: kaibo's own `reqwest` client, handed to
//! rig through `ReqwestClient`, talking to a local server that speaks the OpenAI
//! chat-completions protocol.
//!
//! Every other offline test stops short of the socket: the scripted harness replaces rig's
//! transport, and the capture tests replace rig's HTTP client. This one keeps all of it —
//! `tls::https_client`, `From<reqwest::Client> for ReqwestClient`, the retry wrapper, rig's
//! decoder, the usage fold — so a wiring mistake in the 0.46 transport split (a client that
//! never connects, a body rig cannot read, a usage counter lost on the way) fails here and
//! not on the first live call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kaibo::config::{Config, ModelRole};
use kaibo::consult::{oneshot, Arm, PhaseContext};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A chat completion the way an OpenAI-compatible server answers one, with a cached-token
/// split so the usage fold has something to lose.
const ANSWER: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"391"},"finish_reason":"stop"}],"usage":{"prompt_tokens":163,"completion_tokens":58,"total_tokens":221,"prompt_tokens_details":{"cached_tokens":100}}}"#;

/// Serve `replies` to successive requests (one connection each, `Connection: close`),
/// recording how many arrived. A reply is `(status line, extra headers, body)`.
async fn serve(
    replies: Vec<(&'static str, &'static str, &'static str)>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        while let Ok((mut sock, _)) = listener.accept().await {
            // Read the request: headers, then `content-length` bytes of body.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_lowercase();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let (status, headers, body) = replies
                .next()
                .unwrap_or(("500 Internal Server Error", "", "out of scripted replies"));
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/v1"), seen)
}

/// The built-in openai-compatible synth arm, aimed at `base_url`.
fn arm_at(base_url: String) -> Arm {
    let cfg = Config::builtin();
    let mut backend = cfg
        .resolve_backend("openai-local")
        .expect("built-in openai backend")
        .clone();
    backend.base_url = Some(base_url);
    backend.key_optional = true;
    backend.request_timeout = Duration::from_secs(10);
    let slot = cfg
        .resolve_cast("openai-local")
        .expect("built-in openai cast")
        .require_slot(ModelRole::Synth)
        .expect("synth slot");
    Arm::from_slot(&backend, slot, ModelRole::Synth, &cfg.defaults).expect("arm builds")
}

#[tokio::test]
async fn an_answer_travels_the_real_transport_with_its_usage() {
    let (base_url, seen) = serve(vec![("200 OK", "", ANSWER)]).await;
    let (answer, usage) = oneshot("what is 17 * 23?", &[], &arm_at(base_url), &PhaseContext::default())
        .await
        .expect("the server answers");

    assert_eq!(answer, "391");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "one request, no retry");
    assert_eq!(usage.input_tokens, Some(163), "prompt tokens survive the fold");
    assert_eq!(usage.output_tokens, Some(58));
    assert_eq!(
        usage.cached_input_tokens,
        Some(100),
        "the cached split reaches the footer's source"
    );
}

#[tokio::test]
async fn a_rate_limit_is_waited_out_and_the_request_sent_again() {
    // `Retry-After: 0` keeps the wait to the jittered backoff (at most a second).
    let (base_url, seen) = serve(vec![
        ("429 Too Many Requests", "retry-after: 0\r\n", "slow down"),
        ("200 OK", "", ANSWER),
    ])
    .await;
    let (answer, _) = oneshot("q", &[], &arm_at(base_url), &PhaseContext::default())
        .await
        .expect("the second attempt answers");

    assert_eq!(answer, "391");
    assert_eq!(seen.load(Ordering::SeqCst), 2, "a 429 costs exactly one extra request");
}

#[tokio::test]
async fn a_rejected_request_is_not_sent_again() {
    let (base_url, seen) = serve(vec![
        ("400 Bad Request", "", r#"{"error":{"message":"bad"}}"#),
        ("200 OK", "", ANSWER),
    ])
    .await;
    let res = oneshot("q", &[], &arm_at(base_url), &PhaseContext::default()).await;

    assert!(res.is_err(), "a 400 is the caller's error");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "a rejection costs one request");
}
