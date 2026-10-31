//! The reasoning-`effort` wire matrix, proven offline against rig's *real* wires.
//!
//! kaibo shapes a per-role `effort` into an `additional_params` blob and hands it to
//! rig. Every unit test around that shaping asserts the shape of kaibo's own blob —
//! which is exactly why a live landmine once sat there unnoticed: nothing handed the
//! blob to rig, so `effort = "xhigh"` on a Gemini slot stayed green in CI and died on
//! every call. (rig 0.42 parsed the blob into a typed struct with a closed set of rungs
//! on two of the six wires. rig 0.46 merges it into the request body as raw JSON on
//! every wire, so that ceiling is gone and the provider alone answers for a rung.)
//!
//! So this suite closes the loop with no key and no network: a fake `HttpClientExt`
//! records the body rig builds, and the test drives a real model for each wire kaibo
//! ships. What comes back is the truth — the request rig would have sent. Three things
//! ride on it:
//!
//! 1. **Every rung reaches the wire unchanged** (`every_rung_reaches_the_wire_as_kaibo_shaped_it`)
//!    on every wire that carries effort, including a rung kaibo cannot rank. This is the
//!    property "thinking/effort params reach the wire unchanged" in one test, and it is
//!    what lets kaibo keep no allowlist of its own.
//! 2. **`none` is a structural off-switch** where a provider ships one
//!    (`effort_none_is_a_structural_off_switch_on_the_wire`).
//! 3. **The silent drops are documented as drops** (`toggle_less_wires_send_no_effort`).
//!    Two wires carry no reasoning knob at all and swallow the setting; the wire says so
//!    here, and `Config::inert_efforts` is what makes an operator hear it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig_core::completion::CompletionRequest;
use rig_core::operation::Completion;
use rig_core::providers::anthropic::AnthropicConfig;
use rig_core::providers::gemini::GeminiConfig;
use rig_core::providers::openai::{wire as openai_wire, OpenAIConfig};
use rig_core::DynModel;
use rig_core::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use rig_core::message::Message;
use serde_json::Value;

use kaibo::consult::{
    hosted_openai_responses_params, known_efforts, ModelShape, ThinkingStyleOverride,
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
            Path::AnthropicAdaptive => "claude-sonnet-5-5",
            Path::AnthropicBudget => "claude-haiku-4-5",
            Path::Gemini => "gemini-3.5-flash",
            Path::DeepSeek => "deepseek-flash",
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
}

fn request(params: Option<Value>) -> CompletionRequest {
    CompletionRequest::new(Message::user("hi"))
        .preamble("ground every claim")
        .max_tokens(4096)
        .additional_params(params)
}

async fn capture(
    model: DynModel<Completion>,
    cap: &Capture,
    params: Option<Value>,
) -> Result<Value, String> {
    // The completion always errors (the capture client has no response to give); what
    // matters is whether a body was built first. A recorded body means rig accepted the
    // blob and serialized the request; nothing recorded means rig refused it, and its
    // message is the one an operator would have seen mid-call.
    let err = model.call(request(params)).await.err();
    match cap.recorded() {
        Some(body) => Ok(body),
        None => Err(err.map(|e| e.to_string()).unwrap_or_default()),
    }
}

/// The model for `path`, sending through the capture transport.
fn model_for(path: Path, cap: &Capture) -> DynModel<Completion> {
    match path {
        Path::AnthropicAdaptive | Path::AnthropicBudget => AnthropicConfig::new("test-key")
            .connect(cap.clone())
            .completion(path.model())
            .into(),
        Path::Gemini => GeminiConfig::new("test-key")
            .connect(cap.clone())
            .completion(path.model())
            .into(),
        Path::DeepSeek => OpenAIConfig::with_key(&openai_wire::DEEPSEEK, "test-key")
            .connect(cap.clone())
            .chat(path.model())
            .into(),
        Path::OpenRouter => OpenAIConfig::with_key(&openai_wire::OPENROUTER, "test-key")
            .connect(cap.clone())
            .chat(path.model())
            .into(),
        Path::OpenaiChat => OpenAIConfig::new("test-key")
            .with_base_url("http://127.0.0.1:1/v1")
            .connect(cap.clone())
            .chat(path.model())
            .into(),
        Path::OpenaiResponses => OpenAIConfig::new("test-key")
            .with_base_url("https://api.openai.com/v1")
            .connect(cap.clone())
            .responses(path.model())
            .into(),
    }
}

/// Drive one path at one rung through rig's real wire and report what reached the
/// wire: `Ok(Some(v))` — the effort landed as `v`; `Ok(None)` — rig built the request
/// but this wire has no reasoning field, so the setting was dropped; `Err(msg)` — rig
/// refused the blob before sending, with its own message.
async fn on_the_wire(path: Path, effort: &str) -> Result<Option<Value>, String> {
    let cap = Capture::default();
    let params = path.shaped(effort);
    let body = capture(model_for(path, &cap), &cap, params).await?;
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
/// same: carried to the provider, which alone decides.
const UNRANKED: &str = "ludicrous";

/// Every rung on every wire reaches the request body exactly as kaibo shaped it — the
/// property that makes kaibo's lack of an effort allowlist safe. For the five wires that
/// carry an effort, the rung must appear verbatim at that wire's lever; the two that
/// carry none must still build the request, with the rung absent
/// (`toggle_less_wires_send_no_effort` owns that half).
///
/// **Audit log.** 0.42 → 0.46: rig used to parse the blob into a typed struct on the
/// Gemini and OpenAI Responses wires, with a closed set of rungs that this test pinned
/// (`minimal low medium high` for Gemini, `none … max` for Responses) and refused
/// anything else before sending. 0.46 merges `additional_params` into the body as raw
/// JSON on every wire (`RawAt::Top`), so those ceilings are gone: Gemini now carries
/// `xhigh` to Google, which answers for it (Google rejects `none`/`xhigh`/`max`, verified
/// live in the 0.42 era). That moves the refusal from kaibo's arm construction to the
/// provider's reply, and `Arm::from_slot` no longer preflights the blob.
#[tokio::test]
async fn every_rung_reaches_the_wire_as_kaibo_shaped_it() {
    for path in Path::ALL {
        for rung in known_efforts().into_iter().chain([UNRANKED]) {
            let landed = on_the_wire(path, rung)
                .await
                .unwrap_or_else(|e| panic!("{path:?} at {rung:?} must reach the wire: {e}"));
            let toggle_less = matches!(path, Path::AnthropicBudget | Path::OpenaiChat);
            if toggle_less {
                assert!(landed.is_none(), "{path:?} drops {rung:?}: {landed:?}");
                continue;
            }
            // The structural off-switch replaces the effort string on two wires.
            if kaibo::consult::is_effort_off(rung)
                && matches!(path, Path::DeepSeek | Path::OpenRouter)
            {
                continue;
            }
            let landed = landed.unwrap_or_else(|| {
                panic!("{path:?}: effort {rung:?} must reach the wire, but was dropped")
            });
            let shaped = path.shaped(rung).expect("an effort-carrying path shapes a blob");
            let lever = match path {
                Path::AnthropicAdaptive => shaped.pointer("/output_config/effort").cloned(),
                Path::Gemini => shaped
                    .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                    .cloned(),
                Path::DeepSeek => shaped.pointer("/reasoning_effort").cloned(),
                Path::OpenRouter | Path::OpenaiResponses => {
                    shaped.pointer("/reasoning/effort").cloned()
                }
                _ => unreachable!("toggle-less paths continue above"),
            };
            assert_eq!(
                Some(landed),
                lever,
                "{path:?} at {rung:?}: the wire carries what kaibo shaped, unchanged"
            );
        }
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
    let body = capture(
        model_for(Path::DeepSeek, &cap),
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
    let body = capture(
        model_for(Path::OpenRouter, &cap),
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
