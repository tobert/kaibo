//! The reasoning-`effort` wire matrix, proven offline against rig's *real* clients.
//!
//! kaibo shapes a per-role `effort` into an `additional_params` blob and hands it to
//! rig. Every unit test around that shaping asserts the shape of kaibo's own blob —
//! which is exactly why a live landmine sat there unnoticed: nothing handed the blob to
//! rig, so `effort = "xhigh"` on a Gemini slot stayed green in CI and died on every
//! call. rig parses the blob into a typed struct on two of the six wires, and those
//! structs have a **closed** set of rungs — a ceiling that is *rig's client*, not the
//! provider's API, and that moves on rig's release schedule rather than the provider's.
//! The two have disagreed before, in both directions.
//!
//! So this suite closes the loop with no key and no network: a fake `HttpClientExt`
//! records the body rig builds, and the test drives a real `CompletionModel` for each
//! wire kaibo ships. What comes back is the truth — the request rig would have sent, or
//! the error rig raises before sending. Three things ride on it:
//!
//! 1. **The ladder each wire accepts is pinned** (`rig_effort_ladders_are_pinned`), so a
//!    rig bump that narrows — or widens — a rung fails here instead of in someone's
//!    consult.
//! 2. **kaibo's preflight agrees with rig, rung for rung**
//!    (`kaibo_preflight_agrees_with_rig`). `Arm::from_slot` asks rig's converter before
//!    the call so a refusal names the cast/slot/backend instead of arriving as a bare
//!    serde line mid-consult. This test is what keeps that preflight honest: it is the
//!    *same* converter, so it can never grow into a second, drifting allowlist.
//! 3. **The silent drops are documented as drops** (`toggle_less_wires_send_no_effort`).
//!    Two wires carry no reasoning knob at all and swallow the setting; the wire says so
//!    here, and `Config::inert_efforts` is what makes an operator hear it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, CompletionRequest};
use rig_core::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use rig_core::message::Message;
use rig_core::OneOrMany;
use serde_json::Value;

use kaibo::consult::{
    accepted_efforts, hosted_openai_responses_params, known_efforts, preflight_params, EffortWire,
    ModelShape, ThinkingStyleOverride,
};
use kaibo::credentials::ProviderKind;

/// An `HttpClientExt` that records the serialized request body and then fails. The
/// failure is the point: everything under test is already built by the time rig hands
/// the request over, so no network, no key, and no canned response are needed.
#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<Value>>>);

impl Capture {
    fn recorded(&self) -> Option<Value> {
        self.0.lock().unwrap().first().cloned()
    }
}

impl HttpClientExt for Capture {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let (_parts, body) = req.into_parts();
        let bytes: Bytes = body.into();
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
        async move { Err(http_client::Error::StreamEnded) }
    }

    // Unreachable for a completion, but the trait requires them. `async fn` can't
    // express the trait's `+ Send + 'static` return bound, so the desugared form stays.
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        async move { Err(http_client::Error::StreamEnded) }
    }

    #[allow(clippy::manual_async_fn)]
    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        async move { Err(http_client::Error::StreamEnded) }
    }
}

/// The six request shapes kaibo builds, each named by the arm `Arm::from_slot` would
/// construct for it. `Anthropic` splits by tier because the tiers differ in the one way
/// this suite is about: adaptive routes the effort, budget has no place to put it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    AnthropicAdaptive,
    AnthropicBudget,
    Gemini,
    DeepSeek,
    OpenRouter,
    /// The generic `openai` kind on `/chat/completions` — every local llama.cpp / Ollama
    /// / gateway backend.
    OpenaiChat,
    /// A backend resolved onto rig's Responses client (hosted OpenAI Platform, or an
    /// explicit `wire = "responses"`), with a reasoning-family model id.
    OpenaiResponses,
}

impl Path {
    const ALL: [Path; 7] = [
        Path::AnthropicAdaptive,
        Path::AnthropicBudget,
        Path::Gemini,
        Path::DeepSeek,
        Path::OpenRouter,
        Path::OpenaiChat,
        Path::OpenaiResponses,
    ];

    fn model(&self) -> &'static str {
        match self {
            Path::AnthropicAdaptive => "claude-sonnet-4-6",
            Path::AnthropicBudget => "claude-haiku-4-5",
            Path::Gemini => "gemini-3.5-flash",
            Path::DeepSeek => "deepseek-v4-pro",
            Path::OpenRouter => "z-ai/glm-5.2",
            Path::OpenaiChat => "gemma-local",
            Path::OpenaiResponses => "gpt-5.6-sol",
        }
    }

    fn kind(&self) -> ProviderKind {
        match self {
            Path::AnthropicAdaptive | Path::AnthropicBudget => ProviderKind::Anthropic,
            Path::Gemini => ProviderKind::Gemini,
            Path::DeepSeek => ProviderKind::DeepSeek,
            Path::OpenRouter => ProviderKind::OpenRouter,
            Path::OpenaiChat | Path::OpenaiResponses => ProviderKind::Openai,
        }
    }

    fn responses_wire(&self) -> bool {
        *self == Path::OpenaiResponses
    }

    /// kaibo's `additional_params` blob for this path at `effort` — built by the same
    /// public shaping calls `Arm::from_slot` makes, so the test can't shape it "nicer"
    /// than production does.
    fn shaped(&self, effort: &str) -> Option<Value> {
        if self.responses_wire() {
            return hosted_openai_responses_params(
                self.model(),
                None,
                effort,
                ThinkingStyleOverride::Auto,
            );
        }
        ModelShape::resolve(self.kind(), self.model(), ThinkingStyleOverride::Auto)
            .to_params(8192, None, None, effort)
    }

    /// The rig wire this path's blob has to survive, as kaibo classifies it.
    fn wire(&self) -> EffortWire {
        EffortWire::resolve(self.kind(), self.responses_wire())
    }
}

fn request(params: Option<Value>) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: Some("ground every claim".into()),
        chat_history: OneOrMany::one(Message::user("hi")),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(4096),
        tool_choice: None,
        additional_params: params,
        output_schema: None,
        // Local observability policy, never serialized into a provider payload, so it
        // cannot affect what this suite measures.
        record_telemetry_content: false,
    }
}

async fn capture<M: CompletionModel>(
    model: M,
    cap: &Capture,
    params: Option<Value>,
) -> Result<Value, String> {
    // The completion always errors (the capture client has no response to give); what
    // matters is whether a body was built first. A recorded body means rig accepted the
    // blob and serialized the request; nothing recorded means rig refused it, and its
    // message is the one an operator would have seen mid-call.
    let err = model.completion(request(params)).await.err();
    match cap.recorded() {
        Some(body) => Ok(body),
        None => Err(err.map(|e| e.to_string()).unwrap_or_default()),
    }
}

/// Drive one path at one rung through rig's real client and report what reached the
/// wire: `Ok(Some(v))` — the effort landed as `v`; `Ok(None)` — rig built the request
/// but this wire has no reasoning field, so the setting was dropped; `Err(msg)` — rig
/// refused the blob before sending, with its own message.
async fn on_the_wire(path: Path, effort: &str) -> Result<Option<Value>, String> {
    let cap = Capture::default();
    let params = path.shaped(effort);
    let body = match path {
        Path::AnthropicAdaptive | Path::AnthropicBudget => {
            let c = rig_core::providers::anthropic::Client::builder()
                .api_key("test-key")
                .http_client(cap.clone())
                .build()
                .expect("anthropic client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
        Path::Gemini => {
            let c = rig_core::providers::gemini::Client::builder()
                .api_key("test-key")
                .http_client(cap.clone())
                .build()
                .expect("gemini client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
        Path::DeepSeek => {
            let c = rig_core::providers::deepseek::Client::builder()
                .api_key("test-key")
                .http_client(cap.clone())
                .build()
                .expect("deepseek client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
        Path::OpenRouter => {
            let c = rig_core::providers::openrouter::Client::builder()
                .api_key("test-key")
                .http_client(cap.clone())
                .build()
                .expect("openrouter client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
        Path::OpenaiChat => {
            let c = rig_core::providers::openai::CompletionsClient::builder()
                .api_key("test-key")
                .base_url("http://127.0.0.1:1/v1")
                .http_client(cap.clone())
                .build()
                .expect("openai chat client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
        Path::OpenaiResponses => {
            let c = rig_core::providers::openai::Client::builder()
                .api_key("test-key")
                .base_url("https://api.openai.com/v1")
                .http_client(cap.clone())
                .build()
                .expect("openai responses client");
            capture(c.completion_model(path.model()), &cap, params).await?
        }
    };
    // Where each wire puts the depth lever, read back out of the serialized body.
    let landed = match path {
        Path::AnthropicAdaptive | Path::AnthropicBudget => {
            body.pointer("/output_config/effort").cloned()
        }
        Path::Gemini => body
            .pointer("/generationConfig/thinkingConfig/thinkingLevel")
            .cloned(),
        Path::DeepSeek => body.pointer("/reasoning_effort").cloned(),
        // OpenRouter's `none` is the structural disable rather than an effort string;
        // either shape counts as "the setting reached the gateway".
        Path::OpenRouter => body
            .pointer("/reasoning/effort")
            .or_else(|| body.pointer("/reasoning/enabled"))
            .cloned(),
        Path::OpenaiChat | Path::OpenaiResponses => body.pointer("/reasoning/effort").cloned(),
    };
    Ok(landed)
}

/// A rung kaibo will never find on any ladder — an operator's typo, or a provider name
/// newer than kaibo's ladder. Both look the same from here, and both must behave the
/// same: passed through where the wire is passthrough, refused where rig is typed.
const UNRANKED: &str = "ludicrous";

/// The ceilings, pinned. rig parses the blob into a typed struct on exactly two wires,
/// and this is what each accepts today. The list is not a claim about the providers —
/// it is a claim about *rig*, the thing that can refuse a rung before the request is
/// ever sent. A bump that moves either ladder lands here first, which is the whole
/// point: the last time one moved, we found out from a failed consult.
///
/// **Audit log.** 0.38.2 → 0.41.0 (2026-08-01): `max` opened on the Responses wire.
/// rig had trailed OpenAI by a rung — the endpoint took `"max"` (probed live: 200,
/// echoed back) while rig's enum lacked the variant — and 0.41 closed the gap, so the
/// ladder is now exactly [`known_efforts`]. Updating this pin was the *only* work the
/// bump needed here, because `accepted_efforts` derives the list by probing rig rather
/// than declaring one.
///
/// Gemini did not move, and should not: `thinkingLevel` is a closed protobuf enum and
/// Google rejects `none`/`xhigh`/`max` (verified live). A bump that "widens" Gemini
/// here is a rig bug, not a feature.
#[tokio::test]
async fn rig_effort_ladders_are_pinned() {
    for path in Path::ALL {
        let mut accepted = Vec::new();
        for rung in known_efforts() {
            if on_the_wire(path, rung).await.is_ok() {
                accepted.push(rung);
            }
        }
        let expected: Vec<&str> = match path.wire() {
            // rig's `gemini_api_types::ThinkingLevel` — no `none`, no `xhigh`, no `max`.
            // This is the landmine: `[defaults] synth_effort = "xhigh"` broke every
            // Gemini cast, and every offline test stayed green.
            EffortWire::Gemini => vec!["minimal", "low", "medium", "high"],
            // rig's `responses_api::ReasoningEffort` — level with OpenAI's own ladder.
            EffortWire::OpenaiResponses => {
                vec!["none", "minimal", "low", "medium", "high", "xhigh", "max"]
            }
            // `#[serde(flatten)]` all the way down: kaibo can express anything and the
            // provider is the one that answers for it.
            EffortWire::Passthrough => known_efforts(),
        };
        assert_eq!(
            accepted, expected,
            "{path:?}: rig's accepted rungs moved — re-read rig's request builder for \
             this wire, then update this pin deliberately"
        );

        // An unrankable rung is refused by exactly the typed wires, and only those.
        let unranked_ok = on_the_wire(path, UNRANKED).await.is_ok();
        assert_eq!(
            unranked_ok,
            path.wire() == EffortWire::Passthrough,
            "{path:?}: a passthrough wire must carry an unknown rung to the provider, \
             and a typed wire must refuse it"
        );
    }
}

/// kaibo asks rig's own converter before building an arm, so a refusal names the cast,
/// the slot and the rungs that wire *does* take instead of surfacing "unknown variant
/// `max`" halfway through a consult. That preflight is only trustworthy if it answers
/// exactly as the live call would — every rung, every wire, including the ones outside
/// any ladder. Any drift here means kaibo has grown a second opinion about efforts,
/// which is the allowlist we deliberately refused to write.
#[tokio::test]
async fn kaibo_preflight_agrees_with_rig() {
    for path in Path::ALL {
        for rung in known_efforts().into_iter().chain([UNRANKED]) {
            let rig_accepts = on_the_wire(path, rung).await.is_ok();
            let kaibo_accepts = preflight_params(path.wire(), path.shaped(rung).as_ref()).is_ok();
            assert_eq!(
                kaibo_accepts, rig_accepts,
                "{path:?} at effort {rung:?}: kaibo's preflight and rig's request \
                 builder disagree"
            );
        }
    }
}

/// The accepted-rung list kaibo shows an operator in a refusal is *derived from rig*,
/// not declared beside it — so it can't be the stale half of a two-ladder problem. Pin
/// it against what the wire actually took.
#[tokio::test]
async fn the_error_message_lists_what_the_wire_really_takes() {
    for path in Path::ALL {
        let mut actual = Vec::new();
        for rung in known_efforts() {
            if on_the_wire(path, rung).await.is_ok() {
                actual.push(rung);
            }
        }
        assert_eq!(
            accepted_efforts(path.wire()),
            actual,
            "{path:?}: the rungs kaibo would offer an operator must be the rungs the \
             wire accepts"
        );
    }
}

/// `effort = "none"` means *off*, and on the two providers that ship a structural
/// off-switch it has to use it. Measured 2026-08-01: DeepSeek bills 160–253 reasoning
/// tokens for `thinking:{"type":"enabled"}` + `reasoning_effort:"none"` — the explicit
/// enable wins and the opt-out costs money while doing nothing. So the body that leaves
/// kaibo must carry the disable and no effort string beside it. Asserted on the
/// serialized request, not the blob, because the blob is what the old tests already
/// watched while this shipped.
#[tokio::test]
async fn effort_none_is_a_structural_off_switch_on_the_wire() {
    let cap = Capture::default();
    let c = rig_core::providers::deepseek::Client::builder()
        .api_key("test-key")
        .http_client(cap.clone())
        .build()
        .expect("deepseek client");
    let body = capture(
        c.completion_model(Path::DeepSeek.model()),
        &cap,
        Path::DeepSeek.shaped("none"),
    )
    .await
    .expect("deepseek accepts the disable");
    assert_eq!(
        body["thinking"]["type"], "disabled",
        "deepseek `none` must reach the wire as the structural disable: {body}"
    );
    assert!(
        body.get("reasoning_effort").is_none(),
        "no zero-effort string beside the disable — that pairing is what billed: {body}"
    );

    let cap = Capture::default();
    let c = rig_core::providers::openrouter::Client::builder()
        .api_key("test-key")
        .http_client(cap.clone())
        .build()
        .expect("openrouter client");
    let body = capture(
        c.completion_model(Path::OpenRouter.model()),
        &cap,
        Path::OpenRouter.shaped("none"),
    )
    .await
    .expect("openrouter accepts the disable");
    assert_eq!(
        body["reasoning"]["enabled"], false,
        "openrouter `none` must reach the gateway as its documented off-switch: {body}"
    );
}

/// Two wires have no reasoning knob at all: Anthropic's legacy budget tier expresses
/// depth as `budget_tokens`, and the generic OpenAI `/chat/completions` shape (every
/// local llama.cpp / Ollama / gateway backend) has nothing to carry it. An `effort`
/// aimed at either evaporates — rig builds a perfectly good request with the setting
/// simply absent, and nothing anywhere fails. That is the drop `Config::inert_efforts`
/// exists to make audible; this pins that it really is a drop, on the wire, for every
/// rung — and that the effort-carrying wires really do carry it.
#[tokio::test]
async fn toggle_less_wires_send_no_effort() {
    for path in Path::ALL {
        let toggle_less = matches!(path, Path::AnthropicBudget | Path::OpenaiChat);
        for rung in ["low", "high"] {
            let landed = on_the_wire(path, rung)
                .await
                .unwrap_or_else(|e| panic!("{path:?} at {rung:?} must reach the wire: {e}"));
            if toggle_less {
                assert!(
                    landed.is_none(),
                    "{path:?}: effort {rung:?} must be absent from the request — it is \
                     dropped, and pretending otherwise is the silent fallback: {landed:?}"
                );
            } else {
                assert!(
                    landed.is_some(),
                    "{path:?}: effort {rung:?} must reach the wire"
                );
            }
        }
        // Sanity on the budget tier specifically: it drops the effort but still sends
        // thinking, so "no effort" never means "no reasoning".
        if path == Path::AnthropicBudget {
            let params = path.shaped("high").expect("budget tier sends thinking");
            assert_eq!(params["thinking"]["type"], "enabled");
        }
    }
}
