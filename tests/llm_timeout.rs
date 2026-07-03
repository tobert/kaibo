//! The LLM-loop deadline: a wedged provider must not hang a tool call forever.
//!
//! The 2026-06-06 incident (docs/issues.md): a local synth call hung ~29 min
//! because a wedged llama-server stayed connected but never emitted a response,
//! and kaibo — having no LLM-call deadline — simply waited. rig's prompt loop is
//! non-streaming, so the only brake is a per-request HTTP timeout on the client.
//!
//! This test stands up a "black hole" server (accepts the TCP connection, then
//! never writes a byte) and points an `openai`-kind backend at it with a short
//! `request_timeout` (the deadline rides the *backend* — it describes the wire —
//! and `Arm::from_slot` bakes it into the arm's HTTP client). With the deadline
//! wired, the call surfaces an error near the deadline; without it, the call
//! hangs — so an outer guard turns a regression into a fast failure instead of a
//! hung suite.

use std::time::{Duration, Instant};

use kaibo::config::{Config, ModelRole};
use kaibo::consult::{oneshot, Arm, PhaseContext};
use tokio::net::TcpListener;

/// Bind an ephemeral port and accept connections forever without ever replying —
/// a provider that's "connected but never emits", the wedge in miniature. The
/// returned handle owns the listener; dropping it at test end stops accepting.
async fn black_hole() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        // Hold each accepted socket open and never write a response.
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    (format!("http://{addr}/v1"), handle)
}

#[tokio::test]
async fn oneshot_aborts_when_the_provider_never_responds() {
    let (base_url, _server) = black_hole().await;

    // The built-in openai backend aimed at the black hole, keyless, with a short
    // deadline so the test is quick but the mechanism is the production one.
    let cfg = Config::builtin();
    let mut backend = cfg
        .resolve_backend("openai-local")
        .expect("built-in openai backend")
        .clone();
    backend.base_url = Some(base_url);
    backend.key_optional = true;
    backend.request_timeout = Duration::from_secs(2);

    let cast = cfg.resolve_cast("openai-local").expect("built-in openai cast");
    let slot = cast
        .require_slot(ModelRole::Synth)
        .expect("openai cast has a synth slot");
    let arm = Arm::from_slot(&backend, slot, ModelRole::Synth, &cfg.defaults)
        .expect("arm against the black hole builds");

    // Outer guard: if the per-request deadline regresses, the call hangs and this
    // fires, failing fast rather than wedging the whole suite.
    let started = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        oneshot(
            "What does the sandbox prevent?",
            &[],
            &arm,
            &PhaseContext::default(),
        ),
    )
    .await;
    let elapsed = started.elapsed();

    let inner = res.expect("the per-request timeout must fire — the call must not hang");
    assert!(
        inner.is_err(),
        "a never-responding provider must surface an error, got Ok"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "should abort near the 2s deadline, took {elapsed:?}"
    );
}

/// The whole-call backstop (`call_deadline`), independent of the per-request wire
/// timeout. The 2026-06-06 fix above rides the *backend* (`request_timeout`) and
/// covers "no bytes ever" — but the 2026-07-02 recurrence hung ~17h *despite* a
/// 900s `request_timeout`, a wedge shape that per-request deadline didn't catch
/// (a stalled body read across rig's split send/bytes; a pooled keep-alive to a
/// wedged server). `call_deadline` is the transport-agnostic ceiling for exactly
/// that: here `request_timeout` is set *generously* (300s) and `call_deadline`
/// *tight* (2s), so a fast abort can only be the call-level backstop — proving it
/// bounds the call even when the wire timeout would let it run for minutes.
#[tokio::test]
async fn call_deadline_bounds_a_call_even_with_a_generous_request_timeout() {
    let (base_url, _server) = black_hole().await;

    let cfg = Config::builtin();
    let mut backend = cfg
        .resolve_backend("openai-local")
        .expect("built-in openai backend")
        .clone();
    backend.base_url = Some(base_url);
    backend.key_optional = true;
    // Generous per-request wire timeout: if this were the only brake, the call would
    // run for minutes. The point is that it is NOT the only brake anymore.
    backend.request_timeout = Duration::from_secs(300);

    let cast = cfg.resolve_cast("openai-local").expect("built-in openai cast");
    let slot = cast
        .require_slot(ModelRole::Synth)
        .expect("openai cast has a synth slot");
    let arm = Arm::from_slot(&backend, slot, ModelRole::Synth, &cfg.defaults)
        .expect("arm against the black hole builds");

    // Tight whole-call ceiling — the independent backstop under test.
    let ctx = PhaseContext {
        call_deadline: Duration::from_secs(2),
        ..PhaseContext::default()
    };

    let started = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        oneshot("What does the sandbox prevent?", &[], &arm, &ctx),
    )
    .await;
    let elapsed = started.elapsed();

    let inner =
        res.expect("call_deadline must fire — the call must not hang for the 300s wire timeout");
    let err = inner.expect_err("a wedged provider must surface an error, got Ok");
    assert!(
        format!("{err:#}").to_lowercase().contains("deadline"),
        "the abort must name the wall-clock deadline: {err:#}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "should abort near the 2s call_deadline, not the 300s wire timeout, took {elapsed:?}"
    );
}
