//! The consult tool-loop engine and orchestration.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rig_agent::agent::hook::{
    AgentHook, CompletionCall, CompletionCallAction, HookContext, ToolResultAction, ToolResultEvent,
};
use rig_agent::agent::AgentBuilder;
use rig_agent::completion::PromptError;
use rig_agent::tool::{DynamicTool, Tool, ToolExecutionError, ToolOutput};
use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, MimeType, ToolChoice, ToolResult,
    ToolResultContent, UserContent,
};
use rig_core::completion::{CompletionModel, Message, Usage};
use rig_core::providers::{anthropic, deepseek, gemini, openai, openrouter};
use rig_core::OneOrMany;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::artifact::SaveArtifact;
use crate::attach::Attachment;
use crate::completion_watch::{watched, CompletionLog, Watched};
use crate::config::{Backend, Defaults, ModelRole, ModelSlot};
use crate::credentials::WireKind;
use crate::explorer::RunKaish;
use crate::progress::{PhaseEvent, ProgressSink};
use crate::sandbox::{KaishWorker, SandboxConfig};
use crate::session::{QaTurn, Sessions};
use crate::sweep_attach::{SweepAttach, SweepAttachSink, SweepConsumer, SweepConsumerKind};
use crate::tool_span::traced;
use crate::view_image::ViewImage;

use super::config::{ConsultConfig, ExploreConfig, PhaseContext};
#[cfg(test)]
use super::prompts::PromptOverrides;
use super::prompts::{
    consult_user_prompt, deliberation_prompt, explorer_attach_directive, resolve_phase_preamble,
    sweep_evidence_block, Phase,
};
use super::shaping::{ModelCaps, ModelShape};

// --- The Arm seam ------------------------------------------------------------

/// The toolset factory a phase loop rebuilds its tools from (once for the main
/// loop, again for the turn-cap finalize turn — see [`run_phase`]).
type ToolFactory<'a> = &'a (dyn Fn() -> Result<Vec<DynamicTool>> + Send + Sync);

/// The eventual `(answer, usage)` a phase loop yields, boxed `Send` for the
/// [`PhaseRunner`] vtable: the model's answer paired with the token [`Usage`] the
/// provider reported for the phase (zero-valued when unreported). A named alias so
/// the vtable signature stays readable — and under clippy's `type_complexity` bar.
type PhaseFuture<'a> = Pin<Box<dyn Future<Output = Result<(String, Usage)>> + Send + 'a>>;

/// The object-safe seam one [`Arm`] runs its loops through: a pre-built
/// completion model erased behind a vtable. rig's provider models are distinct
/// concrete types; monomorphizing every phase combination would be a kinds² macro
/// product (the decided plumbing fork, `docs/casts.md`) — the calls are
/// network-bound, so dynamic dispatch here is free. The one implementation,
/// [`ModelArm`], forwards to the generic [`run_phase`], which stays the
/// offline-testable primitive.
trait PhaseRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
    fn run_phase<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        temperature: Option<f64>,
        initial_prompt: Message,
        max_turns: usize,
        params: Option<&'a Value>,
        progress: &'a dyn ProgressSink,
        make_tools: ToolFactory<'a>,
        break_on_tool_images: bool,
    ) -> PhaseFuture<'a>;

    /// One completion, straight to the provider — no agent, no tool loop. The
    /// single-shot phases (`oneshot`, `deliberate`'s direct lane) are exactly one
    /// upstream request by definition, so they say so instead of asking a tool loop
    /// to arrive at the same place with an empty toolset. See [`run_completion`].
    fn complete<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        temperature: Option<f64>,
        prompt: Message,
        params: Option<&'a Value>,
    ) -> PhaseFuture<'a>;
}

/// The concrete pre-built completion model behind the [`PhaseRunner`] vtable.
/// Holding the *model* (not a client + id) lets a provider-specific constructor
/// ride along — the OpenRouter arm's explicit prompt caching is set once here and
/// every loop turn inherits it.
struct ModelArm<M> {
    model: M,
    /// The model id, for the `run_phase` span — the vtable erases the type, and
    /// the telemetry must keep naming which model ran (it's how spend and
    /// behavior get attributed per model when reading traces).
    name: String,
}

impl<M> PhaseRunner for ModelArm<M>
where
    M: CompletionModel + 'static,
{
    fn run_phase<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        temperature: Option<f64>,
        initial_prompt: Message,
        max_turns: usize,
        params: Option<&'a Value>,
        progress: &'a dyn ProgressSink,
        make_tools: ToolFactory<'a>,
        break_on_tool_images: bool,
    ) -> PhaseFuture<'a> {
        Box::pin(run_phase(
            &self.model,
            &self.name,
            preamble,
            max_tokens,
            temperature,
            initial_prompt,
            max_turns,
            params,
            progress,
            make_tools,
            break_on_tool_images,
        ))
    }

    fn complete<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        temperature: Option<f64>,
        prompt: Message,
        params: Option<&'a Value>,
    ) -> PhaseFuture<'a> {
        Box::pin(async move {
            run_completion(
                &self.model,
                &self.name,
                &CompletionLog::new(),
                preamble,
                max_tokens,
                temperature,
                prompt,
                params,
            )
            .await
        })
    }
}

/// One resolved phase arm: its own client + model + request params + caps. The
/// unit `consult`/`oneshot` (and the nested `explore′`) receive — they never learn
/// about backends or casts. The server resolves a cast's slots into arms
/// ([`Arm::from_slot`]); tests inject any [`CompletionClient`] (the scripted
/// offline one included) via [`Arm::new`], which is what keeps the mock harness
/// driving the *real* loop with no network.
#[derive(Clone)]
pub struct Arm {
    runner: Arc<dyn PhaseRunner>,
    /// The model id this arm addresses (diagnostics; the runner carries its own).
    pub model: String,
    /// Output headroom for this arm's completions. **Thinking is on**, so
    /// reasoning eats this budget — it sits well above the thinking budget baked
    /// into `params`, validated at config load.
    pub max_tokens: u64,
    /// Optional typed temperature for providers whose current client reads it
    /// from rig's core request field rather than flattened `additional_params`.
    /// Today this is hosted OpenAI Responses only; the legacy/generic paths keep
    /// their existing provider-shaped params.
    pub temperature: Option<f64>,
    /// The resolved `additional_params` blob (thinking + sampling + effort), fit
    /// to this arm's model by [`ModelShape`]. `None` when nothing is sent.
    pub params: Option<Value>,
    /// What this arm's model can perceive — the seam toolset assembly reads (a
    /// vision arm gets `view_image` when vision-in lands; a blind one never sees
    /// the tool).
    pub caps: ModelCaps,
}

impl std::fmt::Debug for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arm")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("params", &self.params)
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

impl Arm {
    /// Wrap an already-constructed client as an arm. The injection seam: the
    /// server's live arms and the tests' scripted ones meet the loop here.
    pub fn new<C>(
        client: C,
        model: impl Into<String>,
        max_tokens: u64,
        params: Option<Value>,
        caps: ModelCaps,
    ) -> Self
    where
        C: CompletionClient + Clone + Send + Sync + 'static,
        C::CompletionModel: 'static,
    {
        let model = model.into();
        Self::from_model(
            client.completion_model(&model),
            model,
            max_tokens,
            params,
            caps,
        )
    }

    /// Wrap a pre-built completion model as an arm — the seam a provider-specific
    /// model constructor (OpenRouter's prompt-caching flag) enters through.
    fn from_model<M>(
        model_impl: M,
        model: impl Into<String>,
        max_tokens: u64,
        params: Option<Value>,
        caps: ModelCaps,
    ) -> Self
    where
        M: CompletionModel + 'static,
    {
        Self::from_model_with_temperature(model_impl, model, max_tokens, None, params, caps)
    }

    /// Wrap a pre-built completion model with an optional typed temperature.
    fn from_model_with_temperature<M>(
        model_impl: M,
        model: impl Into<String>,
        max_tokens: u64,
        temperature: Option<f64>,
        params: Option<Value>,
        caps: ModelCaps,
    ) -> Self
    where
        M: CompletionModel + 'static,
    {
        let model = model.into();
        Self {
            runner: Arc::new(ModelArm {
                model: model_impl,
                name: model.clone(),
            }),
            model,
            max_tokens,
            temperature,
            params,
            caps,
        }
    }

    /// Resolve one cast slot into a live arm: construct the backend's rig client
    /// (resolving its key, plus base URL for an OpenAI-compatible one) and fit
    /// the request shape to the *slot's* model with the *slot's* tunables (each
    /// falling back to the per-role `[defaults]`). This is the single place the
    /// four concrete client types live; a cast whose phases straddle any
    /// capability line — different kinds, even — is fit per-arm by construction.
    pub fn from_slot(
        backend: &Backend,
        slot: &ModelSlot,
        role: ModelRole,
        defaults: &Defaults,
    ) -> Result<Self> {
        let t = slot.tunables(role, defaults);
        let shape = ModelShape::resolve(backend.kind, &slot.id, t.thinking_style);
        // Re-assert the budget rule at the single live construction point: config
        // load validates every *configured* slot, but per-call overrides build
        // bare slots that never saw it — an inverted pair must be the same
        // keyworded boundary error here, not a provider 400 mid-call. Gate on the
        // shape's budget sink, not the kind: only a style that puts a budget on the
        // wire can starve the answer — Gemini (`thinkingLevel`) and Anthropic adaptive
        // (`output_config.effort`) carry no budget. Checked before key resolution so it
        // fires with no key configured.
        if shape.sinks_thinking_budget() && t.thinking_budget >= t.max_tokens {
            return Err(anyhow!(
                "model {:?} (backend {:?}): thinking_budget ({}) must be < max_tokens \
                 ({}) — reasoning would starve the answer (Anthropic rejects it \
                 outright)",
                slot.id,
                backend.name,
                t.thinking_budget,
                t.max_tokens
            ));
        }
        let params = shape.to_params(
            t.thinking_budget,
            Some(t.temperature),
            Some(t.top_p),
            &t.effort,
        );
        // Interactive wire selection: an explicit per-backend `wire` wins; unset
        // falls back to the endpoint-exact heuristic (today's behavior). This is
        // deliberately `uses_responses_wire`, not the narrower `is_hosted_openai` —
        // that one gates batch eligibility and must stay endpoint-exact (see its
        // doc in `config.rs`), while this call site only builds the interactive
        // request shape.
        let responses_wire = backend.uses_responses_wire();
        let params = if responses_wire {
            super::shaping::hosted_openai_responses_params(&slot.id, Some(t.top_p), &t.effort)
        } else {
            params
        };
        let typed_temperature = if responses_wire {
            super::shaping::hosted_openai_responses_temperature(&slot.id, t.temperature)
        } else {
            None
        };
        // OpenRouter drops rig's native `max_tokens` (see `inject_output_budget`), so
        // the budget must ride `additional_params` as `max_completion_tokens`. A no-op
        // for every other kind, whose `max_tokens` rig sends itself.
        let params = super::shaping::inject_output_budget(backend.kind, params, t.max_tokens);
        // OpenRouter routing honors the backend's data policy — deny by default, so
        // source never reaches a data-collecting upstream host without an explicit
        // config opt-in. A no-op for every other kind.
        let params =
            super::shaping::inject_provider_prefs(backend.kind, params, backend.data_collection);
        // Preflight the blob through rig's OWN request builder for this wire. Two of the
        // six wires parse it into a typed struct whose reasoning rungs are a closed enum,
        // so an effort rig can't name dies *inside rig* at call time with a bare serde
        // line ("unknown variant `max`") that says nothing about which slot of which cast
        // asked for it. Asking here — the single live construction point — turns that into
        // a boundary error naming the slot and the rungs that wire does take. It stays a
        // preflight, not a policy: the accepted list is read back out of rig, so a rig bump
        // that adds a rung needs no kaibo change, and effort stays a passthrough string
        // everywhere rig lets it be one.
        let wire = super::shaping::EffortWire::resolve(backend.kind, responses_wire);
        if let Err(detail) = super::shaping::preflight_params(wire, params.as_ref()) {
            return Err(anyhow!(
                "model {:?} (backend {:?}, {} slot): effort {:?} is refused by rig's {} \
                 request builder before the request leaves kaibo. That ceiling is \
                 rig-core's typed client, not the provider's API — this wire accepts {}. \
                 Set the slot's `effort` (or `[defaults]`) to one of those, or point the \
                 slot at a backend whose wire passes effort through. (rig: {detail})",
                slot.id,
                backend.name,
                role.key(),
                t.effort,
                wire.label(),
                super::shaping::accepted_efforts(wire).join(" | "),
            ));
        }
        let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);

        // One HTTP backend carrying the per-request deadline, built by the shared
        // `crate::tls::https_client` (ring installed, `rustls-no-provider`, no OpenSSL/C —
        // the one client-build site). It bounds the otherwise-brakeless non-streaming call
        // (the 2026-06-06 wedge; see the helper's doc). Injected via rig's `.http_client(..)`.
        let http = crate::tls::https_client(backend.request_timeout)?;

        // A media backend has no completion model to build an arm from — it is reached
        // through the media seam (`crate::media::MediaArm`) on a media cast slot, never
        // through rig. Config validation refuses a reasoning slot pointed at one, so
        // this is the belt to those braces: if the two ever disagree, fail loudly here
        // rather than construct a nonsense arm. Classifying up front also keeps the
        // construction match below on `WireKind`, where a media arm cannot exist.
        let wire = match backend.kind.class() {
            crate::credentials::ProviderClass::Wire(w) => w,
            crate::credentials::ProviderClass::Media(_) => anyhow::bail!(
                "backend {:?} is kind `{}`, which generates media and cannot staff a \
                 reasoning slot — point `explorer`/`synth` at a completion backend, \
                 and use `image = \"{}/<model>\"` for generation",
                backend.name,
                backend.kind.canonical_name(),
                backend.name
            ),
        };
        match wire {
            WireKind::Anthropic => {
                // base_url is optional here (unlike the openai kind): unset dials
                // rig's built-in https://api.anthropic.com; set, it points at an
                // Anthropic-Messages-API-compatible gateway/proxy instead.
                let key = backend.resolve_key()?;
                let mut builder = anthropic::Client::builder().api_key(&key);
                if let Some(base_url) = backend.base_url.as_deref() {
                    builder = builder.base_url(base_url);
                }
                let client = builder
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("anthropic client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            WireKind::DeepSeek => {
                let key = backend.resolve_key()?;
                let client = deepseek::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("deepseek client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            WireKind::Gemini => {
                // base_url is optional here (unlike the openai kind): unset dials
                // rig's built-in https://generativelanguage.googleapis.com; set, it
                // points at a Gemini-API-compatible gateway/proxy instead. Same
                // pattern as the Anthropic arm above — the contract is a HOST ROOT,
                // since rig's builder appends its own versioned path
                // (`/v1beta/models/...`) rather than taking one baked in.
                let key = backend.resolve_key()?;
                let mut builder = gemini::Client::builder().api_key(&key);
                if let Some(base_url) = backend.base_url.as_deref() {
                    builder = builder.base_url(base_url);
                }
                let client = builder
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("gemini client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            WireKind::OpenRouter => {
                // A keyed gateway with a *fixed* endpoint (rig pins the base URL), so —
                // unlike the openai kind — there is no base_url to resolve. `with_app_identity`
                // stamps the X-OpenRouter-Title / HTTP-Referer headers so kaibo's traffic is
                // identifiable in the OpenRouter dashboard.
                let key = backend.resolve_key()?;
                let client = openrouter::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .with_app_identity("kaibo", "https://github.com/tobert/kaibo")
                    .build()
                    .map_err(|e| anyhow!("openrouter client init: {e}"))?;
                let model = Self::openrouter_completion_model(&client, &slot.id);
                Ok(Self::from_model(model, &slot.id, t.max_tokens, params, caps))
            }
            WireKind::Openai => {
                // Any OpenAI-compatible endpoint, addressed by the backend's base
                // URL. The key is optional for a keyless backend: `resolve_key`
                // returns the configured key or a placeholder the server ignores.
                let base_url = backend.resolved_base_url();
                let key = backend.resolve_key()?;
                if responses_wire {
                    let client = openai::Client::builder()
                        .api_key(&key)
                        .base_url(&base_url)
                        .http_client(http)
                        .build()
                        .map_err(|e| anyhow!("openai responses client init at {base_url}: {e}"))?;
                    Ok(Self::from_model_with_temperature(
                        client.completion_model(&slot.id),
                        &slot.id,
                        t.max_tokens,
                        typed_temperature,
                        params,
                        caps,
                    ))
                } else {
                    let client = openai::CompletionsClient::builder()
                        .api_key(&key)
                        .base_url(&base_url)
                        .http_client(http)
                        .build()
                        .map_err(|e| anyhow!("openai client init at {base_url}: {e}"))?;
                    Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
                }
            }
        }
    }

    /// Build the OpenRouter arm's completion model: explicit prompt caching ON.
    /// rig marks the system prompt with a `cache_control: ephemeral` breakpoint,
    /// so Anthropic-upstream slugs bill the resident preamble at cache-read rates
    /// instead of full input price every turn; implicit-caching upstreams
    /// (DeepSeek/GLM/Kimi/Gemini/OpenAI) ignore the marker. The growing
    /// transcript is not marked — an upstream rig limitation, tracked in
    /// docs/issues.md. Kept as a named seam so the construction is unit-testable, and
    /// `from_slot` must route through here.
    ///
    /// Generic over the HTTP backend so a test can drive it with a capture transport
    /// and read the marker off the serialized body — the only place that answers
    /// whether this constructor did its job, since rig exposes no readable flag.
    fn openrouter_completion_model<H>(
        client: &openrouter::Client<H>,
        id: &str,
    ) -> openrouter::CompletionModel<H>
    where
        openrouter::Client<H>: CompletionClient<CompletionModel = openrouter::CompletionModel<H>>,
    {
        client.completion_model(id).with_prompt_caching()
    }

    /// Does a `view_image` on this arm need the user-turn rewrite? True exactly when
    /// the model can *see* but its transport can't carry the image in a tool result
    /// (an OpenAI VLM) — the predicate [`run_phase`]'s break-rewrite-resume gate reads.
    /// A blind arm never sees `view_image`, so this is false there regardless.
    fn rewrites_tool_images(&self) -> bool {
        self.caps.vision && !self.caps.tool_result_images
    }

    /// Run one bounded tool loop on this arm: its client, model, params, and
    /// `max_tokens`, with the caller's preamble/prompt/turn-cap/toolset. Returns the
    /// model's answer paired with the token [`Usage`] the provider reported for this
    /// phase (zero-valued when the provider reported none, or on the undercounted
    /// exceptional paths — see [`run_phase`]).
    pub(crate) async fn run(
        &self,
        preamble: &str,
        initial_prompt: Message,
        max_turns: usize,
        progress: &dyn ProgressSink,
        make_tools: ToolFactory<'_>,
    ) -> Result<(String, Usage)> {
        self.runner
            .run_phase(
                preamble,
                self.max_tokens,
                self.temperature,
                initial_prompt,
                max_turns,
                self.params.as_ref(),
                progress,
                make_tools,
                self.rewrites_tool_images(),
            )
            .await
    }

    /// Ask this arm one question and take its answer: a single completion with this
    /// arm's params and `max_tokens`, no tools and no loop. The single-shot seam
    /// behind `oneshot` and `deliberate`'s direct lane — prompt in, answer out, one
    /// upstream request. Returns the answer paired with the provider's reported
    /// [`Usage`] (zero-valued when it reported none), exactly as [`run`](Self::run) does.
    pub(crate) async fn complete(
        &self,
        preamble: &str,
        prompt: Message,
    ) -> Result<(String, Usage)> {
        self.runner
            .complete(
                preamble,
                self.max_tokens,
                self.temperature,
                prompt,
                self.params.as_ref(),
            )
            .await
    }
}

/// The result of a consult: the final answer, the explorer's report (kept so
/// callers can inspect/debug the hand-off, and for future session storage), the
/// token [`Usage`] the whole consult reported — the synth loop plus every delegated
/// `explore′` sweep, summed. Zero-valued when no provider reported usage (the
/// existing "unknown" sentinel), so a footer can tell "cost this much" from "the
/// backend told us nothing".
///
/// `warnings` carries non-fatal, caller-visible notices about the turn that are
/// **separate from the answer text** — today only a failed session `record()` (the
/// answer is paid-for and stands, but the turn won't replay next time). Kept off
/// `answer` deliberately: a machine consumer of the answer (e.g. `jq -r .answer` on
/// the CLI `--json` envelope) must get the model's words uncorrupted by kaibo's own
/// injected notes. Each front door decides how to surface them — the MCP server
/// appends them to the result text (client-visible behavior unchanged), the CLI
/// prints them to stderr (prose) or as a `warnings` array (`--json`).
#[derive(Debug, Clone)]
pub struct ConsultOutput {
    pub answer: String,
    pub report: String,
    pub usage: Usage,
    pub warnings: Vec<String>,
}

/// The forced-finish instruction we append when a phase exhausts its turn cap.
/// Deliberately repeated front and back: weaker/local models latch onto the most
/// recent instruction, and a model that just spent every turn calling tools needs
/// firm, redundant steering to stop and write. Positive framing where it counts
/// ("write your full response now") bracketed by the hard constraint ("no more
/// tools"), per the `positive-prompt-framing` discipline.
const FINALIZE_NOTE: &str = "\
STOP — you have reached your research limit and may not call any more tools. Using \
only the evidence you have already gathered in this conversation, write your \
COMPLETE final response now, with its concrete `file:line` citations. Where the \
evidence runs out, say so plainly: naming what you do not know is part of a complete \
answer. Do not call any tool. Do not ask to continue. Write the full answer (or \
curated report) from what you already have.";

/// The forced-finish instruction for a phase that **stopped early** without writing
/// its answer — distinct from [`FINALIZE_NOTE`], which is about running out of turns.
/// A model that quit mid-investigation has budget left and needs to hear that it
/// simply owes the write-up, not that it has been cut off. Same positive framing and
/// front-and-back repetition, different fact.
const EMPTY_ANSWER_NOTE: &str = "\
Your last turn contained no answer text — the response came back empty. You have \
already gathered evidence in this conversation, so nothing more needs investigating. \
Using only that evidence, write your COMPLETE final response now, with its concrete \
`file:line` citations. Where the evidence runs out, say so plainly: naming what you \
do not know is part of a complete answer, not a reason to keep investigating. Do not \
call any tool. Do not ask to continue. Write the full answer (or curated report) from \
what you already have.";

/// Build the forced final turn from a partial transcript.
///
/// Returns `(history, prompt)` for one more constrained completion: the prompt is
/// the conversation's last message with `note` appended (so the model reads the
/// "answer now" instruction last), and `history` is everything before it. The note is
/// a parameter because the two forced-finish paths mean different things —
/// [`FINALIZE_NOTE`] (out of turns) vs. [`EMPTY_ANSWER_NOTE`] (stopped without
/// answering) — and telling a model it hit a limit it did not hit is a lie that
/// shapes its answer.
///
/// The transcript's last message is almost always the user's tool-results turn (the
/// loop broke just as the model was about to call yet another tool), so the note
/// rides along inside that same user message — we never emit two user turns back to
/// back, which some providers reject. If the transcript somehow ends on an assistant
/// turn, the note becomes a fresh trailing user turn instead (valid after an
/// assistant message). Pure and offline-testable.
fn finalize_prompt(mut chat_history: Vec<Message>, note: &str) -> (Vec<Message>, Message) {
    match chat_history.pop() {
        Some(Message::User { mut content }) => {
            content.push(UserContent::text(note));
            (chat_history, Message::User { content })
        }
        Some(other) => {
            chat_history.push(other);
            (chat_history, Message::user(note))
        }
        None => (Vec::new(), Message::user(note)),
    }
}

// --- the empty-answer guard ---------------------------------------------------

/// Does this transcript show the model actually *gathered evidence*? True when any
/// user turn carries a tool result — i.e. the model called something and got output
/// back. This is the gate on [`run_phase`]'s one forced re-ask after an empty answer.
///
/// **Why the gate is shaped this way — do not "simplify" it into an unconditional
/// retry.** A forced "write it now" turn handed to a model that has gathered *nothing*
/// invites it to comply anyway, and what it produces is a confident, ungrounded answer
/// on a lane whose entire product is grounded citation. So the case where a retry is
/// least likely to succeed is also the case where succeeding is *dangerous*: a
/// fabricated review is worse than an error, in the same way a silent empty is. With
/// evidence in hand, the same turn is safe and usually sufficient — it asks a model
/// that already did the work to write up what it found (the 2026-08-01 deepseek run:
/// an explorer report delivered, fifteen greps returned, then a 14-token terminal turn
/// with no text).
///
/// **What this does and does not prove.** rig folds a *failed* tool call into a tool
/// result too — it stringifies the error and hands it back as the result
/// (`rig-core/src/agent/prompt_request/mod.rs:1019-1025`) — so a tool result is proof
/// the model got output back, not proof the output was useful. That is the right line
/// anyway: a transcript of nothing but tool errors still yields an honest grounded
/// answer ("I could not read X"), which is a fine thing to have asked for. What the
/// gate excludes is the genuinely dangerous shape — zero tool interaction, pure
/// generation, nothing to cite.
fn transcript_has_tool_results(history: &[Message]) -> bool {
    history.iter().any(|m| match m {
        Message::User { content } => content
            .iter()
            .any(|c| matches!(c, UserContent::ToolResult(_))),
        _ => false,
    })
}

/// Fail a phase that produced no answer text, carrying the diagnostics that say what
/// was burned to produce nothing.
///
/// The invariant this enforces: **an empty final answer is never a successful call.**
/// A review that silently did not happen is worse than one that errors, because the
/// caller merges on it. Same vocabulary as the batch lane's long-standing gate
/// (`crate::batch`'s `finish_gated_answer`: "a completed-but-empty answer is no
/// answer") — one concept, one wording, two lanes.
///
/// `finish_reason` comes off the phase's [`CompletionLog`] — the provider's own word
/// for how its last completion ended, read from the raw response the [`Watched`]
/// wrapper records. `None` means the provider reported none, and that is stated as
/// absence rather than implied to be a clean stop: on some wires (OpenAI's Responses
/// API) reporting nothing *is* the normal completion signal, so absence carries no
/// verdict either way.
fn empty_answer_error(
    model: &str,
    turns: usize,
    max_turns: usize,
    usage: &Usage,
    finish_reason: Option<&str>,
    detail: &str,
) -> anyhow::Error {
    let reason = match finish_reason {
        Some(r) => format!("finish_reason \"{r}\" reported by the provider"),
        None => "no finish_reason reported by the provider (normal on some wires)".to_string(),
    };
    anyhow!(
        "model {model} returned an EMPTY answer — {detail}. It is not a result: a \
         completed-but-empty answer is no answer, and returning it would hand you a \
         review that never happened. Diagnostics: {turns} of {max_turns} turns used, \
         {} input tokens ({} cached), {} output tokens, {} reasoning tokens reported; \
         {reason}. Retry, or try the same question on a different cast.",
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
        usage.reasoning_tokens,
    )
}

// --- an image-bearing tool result on the user-turn channel (the openai VLM path) --

/// The cancellation reason [`ToolImageBreakHook`] terminates with, so [`run_phase`]
/// can tell *its* deliberate break from any other `PromptCancelled` rig might raise
/// (a lost prompt, an empty tool batch). An internal sentinel; never shown to a model.
const TOOL_IMAGE_BREAK: &str = "kaibo:view_image_break";

/// The text a rewritten `view_image` tool result carries when its own note is somehow
/// absent — enough to satisfy the `tool_use → tool_result` pairing every provider
/// requires. The image itself rides the separate user turn the rewrite inserts.
const TOOL_IMAGE_ACK: &str = "Loaded the requested image; it is shown in the next message.";

/// Breaks the managed tool loop at the turn boundary after a `view_image` ran, so
/// [`run_phase`] can move the image onto the **user-turn** channel for a transport
/// that can't carry it in a tool result (an OpenAI VLM).
///
/// **Why flag now, stop next — not mid-turn.** A single assistant turn can call
/// `view_image` *and* `run_kaish` together; stopping the instant `view_image` returns
/// would drop the other tool's result and orphan its `tool_use`. And a stop from
/// `on_tool_result` hands back a transcript snapshotted *before* the turn's results are
/// folded into the run's messages, so it wouldn't even carry the image we came for. So
/// we only *set a flag* on `on_tool_result`, and stop on the **next**
/// `on_completion_call` — the point where rig has written every tool result of the
/// triggering turn into the transcript and the stop returns it complete. Disabled
/// (`enabled == false`) every callback is a no-op, so installing it on a transport
/// that carries tool-result images (Anthropic/Gemini) is byte-for-byte the old path.
#[derive(Clone)]
struct ToolImageBreakHook {
    enabled: bool,
    /// Set once a `view_image` tool result lands this turn; read at the next
    /// completion call. Interior mutability because `AgentHook`'s callbacks are `&self`
    /// and rig runs a turn's tools concurrently.
    saw_tool_image: Arc<AtomicBool>,
}

impl ToolImageBreakHook {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            saw_tool_image: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AgentHook for ToolImageBreakHook {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if self.enabled && tool_output_carries_image(event.presentation) {
            self.saw_tool_image.store(true, Ordering::SeqCst);
        }
        ToolResultAction::Keep
    }

    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        _event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        if self.enabled && self.saw_tool_image.load(Ordering::SeqCst) {
            CompletionCallAction::Stop(TOOL_IMAGE_BREAK.to_string())
        } else {
            CompletionCallAction::Continue
        }
    }
}

/// Does this tool output carry an image part? Keys the break hook on the RESULT, not
/// the tool's name — fully general, so the next image-bearing tool (`view_image`
/// today, and a sweep's `explore` result carrying a routed image) needs no new `if`.
/// Typed, not sniffed: rig 0.41 hands the hook the model-visible [`ToolOutput`]
/// whole, so the check reads the declared content parts — the same declared-image
/// contract [`crate::view_image::ViewImage`] and [`crate::consult::RunExplore`] emit
/// on.
fn tool_output_carries_image(output: &ToolOutput) -> bool {
    output
        .as_content()
        .iter()
        .any(|c| matches!(c, ToolResultContent::Image(_)))
}

/// Rewrite a transcript so every image-bearing tool result rides the **user-turn**
/// channel instead of the tool-result channel — `view_image`, or a delegated
/// `explore` sweep that routed an image via `attach`. For each such result that
/// still carries an image: keep its text as a short ack (so the `tool_use → tool_result`
/// pairing stays valid) and emit a separate, tool-result-free `Message::User { [Image] }`
/// right after that user message — the bytes the model now sees on a channel OpenAI
/// accepts. Every other block (assistant text/thinking, other tools' use/result pairs)
/// is preserved verbatim, so no `tool_use` is left unanswered.
///
/// **A separate message, never mixed.** rig's openai converter drops every non-tool
/// part from a user turn that *also* carries tool results (`openai/completion/mod.rs`
/// ~:618) — an image left in the tool-results message would vanish with no error, the
/// exact silent drop we refuse. Hence its own message.
///
/// Idempotent: it triggers only on a tool result that *still holds an image*, so a
/// result already acked to text (an earlier break) and an already-inserted image
/// message both pass through untouched — safe to run after every break. Pure and
/// offline-testable.
fn rewrite_tool_image_history(history: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    for msg in history {
        let content = match msg {
            Message::User { content } => content,
            other => {
                out.push(other);
                continue;
            }
        };

        let mut new_parts: Vec<UserContent> = Vec::new();
        // Images pulled out of this turn's image-bearing tool results, re-emitted as
        // their own user messages immediately after (one per image), preserving order.
        let mut extracted: Vec<Image> = Vec::new();

        for part in content {
            match part {
                // Key on the RESULT still carrying an image — not the tool's name or
                // id. Fully general: the next image-bearing tool (today: `view_image`
                // and a sweep's `explore` result carrying a routed image) needs no
                // edit here. A result already acked to text (an earlier break, or an
                // already-inserted image turn) holds no `ToolResultContent::Image`, so
                // it falls through untouched — the idempotency that makes re-running
                // this safe.
                UserContent::ToolResult(tr)
                    if tr
                        .content
                        .iter()
                        .any(|rc| matches!(rc, ToolResultContent::Image(_))) =>
                {
                    let ToolResult {
                        id,
                        call_id,
                        content,
                    } = tr;
                    // Split the result into its text (the load note → ack) and its
                    // image(s) (→ a user turn each).
                    let mut texts: Vec<ToolResultContent> = Vec::new();
                    for rc in content {
                        match rc {
                            ToolResultContent::Image(img) => extracted.push(img),
                            text => texts.push(text),
                        }
                    }
                    let content = OneOrMany::many(texts).unwrap_or_else(|_| {
                        OneOrMany::one(ToolResultContent::text(TOOL_IMAGE_ACK))
                    });
                    new_parts.push(UserContent::ToolResult(ToolResult {
                        id,
                        call_id,
                        content,
                    }));
                }
                other => new_parts.push(other),
            }
        }

        // Re-emit the (possibly rewritten) tool-results message, then each extracted
        // image as its own tool-result-free user message — the load-bearing separation.
        // Each input part maps to exactly one `new_parts` entry, so an input `User`
        // turn (always non-empty) yields a non-empty `new_parts` — `many` can't fail.
        // Assert it rather than silently skipping: if a future refactor breaks that
        // invariant we want a crash, not a quietly dropped message.
        let content = OneOrMany::many(new_parts)
            .expect("a non-empty user turn maps part-for-part to a non-empty result");
        out.push(Message::User { content });
        for img in extracted {
            out.push(Message::User {
                content: OneOrMany::one(UserContent::Image(img)),
            });
        }
    }
    out
}

/// Count the model turns a transcript represents — one assistant message per
/// completion that produced output. The view_image break re-enters the loop with a
/// fresh `max_turns`, so rig's internal turn counter resets each resume; deriving
/// turns-spent from the transcript (rig's history carries no `turns_used`) is what
/// stops a model that loops `view_image` from refreshing its budget every break.
fn count_model_turns(history: &[Message]) -> usize {
    history
        .iter()
        .filter(|m| matches!(m, Message::Assistant { .. }))
        .count()
}

/// Split a rewritten transcript for re-entry into the managed loop: the trailing
/// message becomes the resume `prompt`, the rest goes to `.with_history(...)`. Mirrors
/// [`finalize_prompt`]'s split (so the original `user_prompt`, already in the history,
/// is never replayed on top of it) but appends no note — this is a normal resume, not
/// a forced finish. The rewritten transcript always carries at least the original
/// prompt, so the empty arm is unreachable defensive code.
fn split_for_resume(mut history: Vec<Message>) -> (Vec<Message>, Message) {
    match history.pop() {
        Some(last) => (history, last),
        None => (Vec::new(), Message::user("")),
    }
}

/// One model loop, parameterized by its toolset: build an agent with `preamble`,
/// hand it the tools `make_tools` builds, and run its bounded tool loop. Generic
/// over the provider.
///
/// The toolset is injected via a *factory* (not a prebuilt `Vec`, and not hardcoded
/// to `run_kaish`) so the same loop is the primitive behind every tool on the
/// surface — `oneshot` ({} — no tools), the recomposed `consult`
/// ({run_kaish, explore′}), and its nested `explore′` ({run_kaish}). The factory matters because of the
/// turn-cap recovery below: a fresh toolset is built for the forced final turn, and
/// A `DynamicTool` toolset is consumed by the builder, so we rebuild rather than share. Each call
/// spawns its own `KaishWorker`(s); the caller owns their lifetime.
///
/// **Turn-cap recovery.** When the model uses every turn without concluding, rig
/// 0.34 returns `MaxTurnsError` carrying the *full transcript* (not the opaque
/// failure the old code mapped to an error). Rather than discard all that work, we
/// run one final constrained turn via [`finalize_after_max_turns`]: the tools stay
/// declared so the accumulated tool_use/tool_result history stays valid, but
/// `ToolChoice::None` forbids new calls — the model must answer from what it has.
///
/// **A tool-result image on the user-turn channel.** When `break_on_tool_images` is
/// set (a vision model whose transport can't carry an image in a tool result — an
/// OpenAI VLM), a [`ToolImageBreakHook`] terminates the loop at the turn boundary
/// after any tool result carries an image — `view_image`, or a delegated `explore`
/// sweep that routed one via `attach`. rig hands back the full transcript via
/// `PromptCancelled`; we rewrite each image-bearing result onto a separate user
/// `Image` turn ([`rewrite_tool_image_history`]) and re-enter the loop with the
/// remaining turn budget. The model now sees the image in user content, the one
/// channel every provider accepts. When unset the hook is inert and this is the old
/// single call.
///
/// **Seeing how each turn ended.** This is [`run_phase_logged`] with a log nobody
/// keeps — reach for that one when the phase's per-turn finish reasons are the point.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named loop input
pub(crate) async fn run_phase<M, F>(
    model: &M,
    model_name: &str,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    initial_prompt: Message,
    max_turns: usize,
    thinking: Option<&Value>,
    progress: &dyn ProgressSink,
    make_tools: F,
    break_on_tool_images: bool,
) -> Result<(String, Usage)>
where
    M: CompletionModel + 'static,
    F: Fn() -> Result<Vec<DynamicTool>>,
{
    run_phase_logged(
        &CompletionLog::new(),
        model,
        model_name,
        preamble,
        max_tokens,
        temperature,
        initial_prompt,
        max_turns,
        thinking,
        progress,
        make_tools,
        break_on_tool_images,
    )
    .await
}

/// [`run_phase`] with the per-turn completion record kept.
///
/// rig's agent layer hands back only what its medium-neutral hook event carries —
/// content and usage — so a phase that stopped early, got truncated, or was refused
/// by a classifier looks identical to one that finished. The provider *did* say which
/// (`finish_reason` / `stop_reason` / `finishReason`); it lives on
/// `CompletionResponse::raw_response`, which the loop discards. Wrapping the model in
/// [`Watched`] puts the record on the one path every completion takes — **including
/// the turns inside the tool loop and the forced final turn**, which no hook reaches —
/// and `log` is the caller's slot for it, readable after this returns whether the
/// phase succeeded or failed.
///
/// The log is per call, never global: a phase makes its own, so concurrent consults
/// never mix. It holds plain data and crosses `.await` freely — nothing to do with the
/// `!Send` kaish kernel, which stays on its `KaishWorker` thread as always.
///
/// [`run_phase`] is this with a log nobody keeps; the *last* turn's finish reason is
/// recorded on the span either way, so the phase's ending is visible in telemetry
/// today.
#[allow(clippy::too_many_arguments)]
// A named parent for rig's GenAI spans: rig's `invoke_agent` checks the current
// span and nests under this one, so a phase's whole model loop (every `chat` turn,
// every `tool` call) hangs off one `run_phase` span carrying the model. Inert
// unless an exporter is attached (telemetry off → no subscriber records it).
#[tracing::instrument(
    name = "run_phase",
    skip_all,
    fields(
        model = %model_name,
        max_turns = max_turns,
        // The resolved wire blob (thinkingLevel/thinkingBudget + effort + sampling) as
        // actually sent — ground truth for "what reasoning shape shipped", which the
        // response-usage spans can only hint at. Empty here, filled in the body once it's
        // known non-None, so a `None` (toggle-less) provider records nothing and
        // telemetry-off stays a no-op.
        gen_ai.request.thinking = tracing::field::Empty,
        // How the phase's LAST completion ended, in the provider's own words. Empty
        // when the provider reported none (or nothing ran), so it exports only when
        // there's something to say.
        gen_ai.response.finish_reason = tracing::field::Empty,
    )
)]
pub(crate) async fn run_phase_logged<M, F>(
    log: &CompletionLog,
    model: &M,
    model_name: &str,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    initial_prompt: Message,
    max_turns: usize,
    thinking: Option<&Value>,
    progress: &dyn ProgressSink,
    make_tools: F,
    break_on_tool_images: bool,
) -> Result<(String, Usage)>
where
    M: CompletionModel + 'static,
    F: Fn() -> Result<Vec<DynamicTool>>,
{
    // Surface the exact reasoning/sampling params this phase ships (constant across the
    // resume loop), so a trace shows whether — and at what depth — thinking was on: the
    // wire truth behind the `chat` spans' `reasoning_tokens`. Inert with no exporter.
    if let Some(t) = thinking {
        tracing::Span::current().record("gen_ai.request.thinking", tracing::field::display(t));
    }
    let result = run_phase_loop(
        &watched(model.clone(), log.clone()),
        log,
        model_name,
        preamble,
        max_tokens,
        temperature,
        initial_prompt,
        max_turns,
        thinking,
        progress,
        make_tools,
        break_on_tool_images,
    )
    .await;
    // Read the ending off the log — after the loop, on every exit path, so a failed
    // phase reports how its last completion ended too.
    if let Some(reason) = log.last_finish_reason() {
        tracing::Span::current().record("gen_ai.response.finish_reason", reason.as_str());
    }
    result
}

/// The bounded tool loop itself, driving whatever model it's handed — in production a
/// [`Watched`] wrapper, so every turn below (main loop, view_image resume, forced
/// finalize) records on its way past.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named loop input
async fn run_phase_loop<M, F>(
    model: &Watched<M>,
    log: &CompletionLog,
    model_name: &str,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    initial_prompt: Message,
    max_turns: usize,
    thinking: Option<&Value>,
    progress: &dyn ProgressSink,
    make_tools: F,
    break_on_tool_images: bool,
) -> Result<(String, Usage)>
where
    M: CompletionModel + 'static,
    F: Fn() -> Result<Vec<DynamicTool>>,
{
    // Loop state across view_image-break resumes. The caller hands us the *assembled*
    // first user turn — a bare `Message::user(prompt)` for every tool-driven phase, or a
    // multi-part turn (oneshot's inlined attachment images beside the text) built in
    // `oneshot`. Keeping the assembly in the caller keeps this engine free of multimodal
    // concerns: it just runs whatever turn it's given, then each view_image break rewrites
    // the transcript and re-enters here (holistic review, Gemini Pro 2026-06-22).
    let mut prompt: Message = initial_prompt;
    let mut history: Vec<Message> = Vec::new();

    loop {
        // Outer turn budget: rig's `max_turns` resets each resume, so subtract the
        // turns already spent (assistant messages in the carried history) — a model
        // that loops `view_image` can't refresh its budget every break.
        let remaining = max_turns.saturating_sub(count_model_turns(&history));
        if remaining == 0 {
            // The whole budget went to view_image breaks — force the finish from what
            // we have, the same shape the turn-cap path uses.
            progress.emit(PhaseEvent::TurnCapReached);
            let mut full = history;
            full.push(prompt);
            return finalize_after_max_turns(
                model,
                log,
                model_name,
                preamble,
                max_tokens,
                temperature,
                thinking,
                make_tools()?,
                full,
                max_turns,
            )
            .await;
        }

        let mut builder = AgentBuilder::new(model.clone())
            .preamble(preamble)
            .max_tokens(max_tokens);
        if let Some(t) = temperature {
            builder = builder.temperature(t);
        }
        // Thinking on (both phases) where the provider takes a request-time toggle.
        if let Some(params) = thinking {
            builder = builder.additional_params(params.clone());
        }
        let agent = builder.dynamic_tools(make_tools()?).build();

        // A fresh hook per loop iteration is load-bearing: its `saw_tool_image` flag
        // must be scoped to *this* turn. Hoisting it out of the loop (or reusing the
        // agent across resumes) would carry a stale flag — breaking on the first
        // completion call of a resume that ran no view_image. Keep it built here.
        // The run yields a `PromptResponse` carrying the token `usage` the provider
        // reported, summed across every turn of *this* run. The clean `Ok` path is the
        // common case — one run, the full count. The two exceptional exits below (turn
        // cap, view_image break) undercount: rig hands back no usage on
        // `MaxTurnsError`/`PromptCancelled`, so the turns spent in a capped or broken
        // run are lost and only the finalize/resumed run's usage survives. Deliberate —
        // recovering it would mean summing per-completion through a hook; documented as
        // a known undercount rather than silently exact.
        let result = agent
            .runner(prompt.clone())
            .history(history.clone())
            .add_hook(ToolImageBreakHook::new(break_on_tool_images))
            .max_turns(remaining)
            .run()
            .await;

        match result {
            Ok(resp) if !resp.output.trim().is_empty() => return Ok((resp.output, resp.usage)),
            // An `Ok` with no answer text. rig treats a textless terminal turn as a clean
            // finish and hands back `output: ""` (its own
            // `prompt_request_stops_cleanly_on_empty_terminal_turn` pins that as intended),
            // so the check has to live here — the one seam every phase runs through, which
            // is what makes `consult`/`oneshot`/`explore`/`deliberate` inherit it at once.
            // The shape that produced it in the wild: a reasoning model whose last choice
            // carried only a `Reasoning` block, which rig's text extraction filters out.
            Ok(resp) => {
                // The spent-so-far usage is real and must survive into whatever we return
                // — this is the `Ok` path, so rig reported it exactly. The retry's usage is
                // ADDED to it below, never substituted (the turn-cap path may legitimately
                // replace, because rig reports none on `MaxTurnsError`).
                let spent = resp.usage;
                // rig builds this response at exactly one place and always attaches the
                // transcript (`with_messages`, `prompt_request/mod.rs:918`), so `Some` is
                // the structural case. `None` is unreachable-by-construction defensive
                // code, and it fails CLOSED: no transcript means no evidence to gate on,
                // and re-asking blind is the fabrication risk the gate exists to refuse.
                let turns = count_model_turns(&history) + resp.completion_calls.len();
                let transcript = resp.messages.unwrap_or_default();
                if !transcript_has_tool_results(&transcript) {
                    return Err(empty_answer_error(
                        model_name,
                        turns,
                        max_turns,
                        &spent,
                        log.last_finish_reason().as_deref(),
                        "it stopped without answering, and its transcript holds no tool \
                         results to write up — asking it to answer anyway would invite an \
                         ungrounded review",
                    ));
                }
                // Evidence in hand: ask for the write-up it owed. **At most once, and
                // structurally so** — this arm returns on every path (the forced turn's
                // answer, or an error), so it never re-enters the loop and cannot stack a
                // second re-ask. That is a stronger guarantee than a `finalized` flag,
                // which a later edit could reset; the shape itself forbids the loop. Pinned
                // by `an_empty_forced_write_up_turn_errors_and_is_never_retried_twice`.
                //
                // **Why not rig-agent's `on_model_turn_finished` + `ModelTurnAction::
                // Retry`?** Evaluated 2026-08-02 (rig-agent 0.41 `hook.rs`) and rejected:
                // the first-class retry is weaker than this recovery on the three counts
                // that matter here. (1) `Retry(Feedback)` cannot constrain the retried
                // turn — [`forced_finish_turn`] sends `ToolChoice::None`, so the model
                // must write rather than spend the nudge on another tool call. (2) A hook
                // retry "consumes the run's existing total model-call budget", so an
                // empty answer at the cap would get no recovery, where this path runs the
                // write-up turn deliberately outside the budget. (3) `Stop(reason)` is a
                // string; [`empty_answer_error`]'s diagnostics (turns, tokens, the
                // provider's finish_reason off the [`CompletionLog`]) would flatten into
                // it. Re-evaluate if rig grows a constrained retry.
                tracing::warn!(
                    model = model_name,
                    turns,
                    output_tokens = spent.output_tokens,
                    "phase returned an empty answer with evidence gathered — forcing one \
                     final write-up turn"
                );
                progress.emit(PhaseEvent::TurnCapReached);
                let mut full = history;
                full.extend(transcript);
                let (answer, retry_usage) = forced_finish_turn(
                    model,
                    preamble,
                    max_tokens,
                    temperature,
                    thinking,
                    make_tools()?,
                    full,
                    EMPTY_ANSWER_NOTE,
                )
                .await
                .map_err(|e| {
                    anyhow!(
                        "model {model_name} returned an empty answer, and the forced \
                         final-answer turn also failed: {e}"
                    )
                })?;
                let usage = spent + retry_usage;
                if answer.trim().is_empty() {
                    return Err(empty_answer_error(
                        model_name,
                        turns + 1,
                        max_turns,
                        &usage,
                        log.last_finish_reason().as_deref(),
                        "it was asked once to write the answer it owed and came back empty \
                         again",
                    ));
                }
                return Ok((answer, usage));
            }
            Err(PromptError::MaxTurnsError { chat_history, .. }) => {
                // The loop hit its cap and is about to write a forced final answer —
                // tell the caller, so a watching client sees "wrapping up" not silence.
                progress.emit(PhaseEvent::TurnCapReached);
                return finalize_after_max_turns(
                    model,
                    log,
                    model_name,
                    preamble,
                    max_tokens,
                    temperature,
                    thinking,
                    make_tools()?,
                    *chat_history,
                    max_turns,
                )
                .await;
            }
            // Our deliberate view_image break. We terminated at the *next* completion
            // call, so every `tool_use` in the triggering turn is already answered in
            // this transcript — co-tool-call orphaning is structurally impossible.
            // Move each view_image image onto its own user turn and resume.
            Err(PromptError::PromptCancelled {
                chat_history,
                reason,
            }) if reason == TOOL_IMAGE_BREAK => {
                let (rest, next) = split_for_resume(rewrite_tool_image_history(chat_history));
                history = rest;
                prompt = next;
            }
            Err(e) => return Err(anyhow!("model loop failed: {e}")),
        }
    }
}

/// One forced final turn: replay the partial transcript and make the model write its
/// answer now, with tools declared (so the history validates) but [`ToolChoice::None`]
/// forbidding any further call. The shared shape behind both recoveries — out of turns
/// ([`finalize_after_max_turns`], [`FINALIZE_NOTE`]) and stopped-without-answering
/// ([`run_phase`], [`EMPTY_ANSWER_NOTE`]) — which differ only in the note they append
/// and the failure they report, so the rig plumbing lives here once.
///
/// Returns rig's error unwrapped so each caller can say what it was recovering *from*;
/// it deliberately does not judge the answer, leaving the empty check to the callers
/// that know which diagnostics belong on it.
#[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
async fn forced_finish_turn<M>(
    model: &M,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    thinking: Option<&Value>,
    tools: Vec<DynamicTool>,
    chat_history: Vec<Message>,
    note: &str,
) -> std::result::Result<(String, Usage), PromptError>
where
    M: CompletionModel + 'static,
{
    let (history, prompt) = finalize_prompt(chat_history, note);
    let mut builder = AgentBuilder::new(model.clone())
        .preamble(preamble)
        .max_tokens(max_tokens)
        .tool_choice(ToolChoice::None);
    if let Some(t) = temperature {
        builder = builder.temperature(t);
    }
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.dynamic_tools(tools).build();
    // max_turns(1): one constrained completion. With tools forbidden the model can't
    // loop, so a single round is enough — and if a provider ignores ToolChoice::None
    // and still calls a tool, we surface that rather than recurse.
    agent
        .runner(prompt)
        .history(history)
        .max_turns(1)
        .run()
        .await
        .map(|resp| (resp.output, resp.usage))
}

/// The forced final turn after a phase hit its turn cap. See [`run_phase`]'s recovery
/// note and [`forced_finish_turn`].
///
/// Spending the entire turn budget and *still* delivering nothing is a failure, not an
/// empty success — so the answer passes the same [`empty_answer_error`] gate the early-
/// stop path uses. There is no re-ask here: this already **is** the re-ask.
#[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
async fn finalize_after_max_turns<M>(
    model: &M,
    log: &CompletionLog,
    model_name: &str,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    thinking: Option<&Value>,
    tools: Vec<DynamicTool>,
    chat_history: Vec<Message>,
    max_turns: usize,
) -> Result<(String, Usage)>
where
    M: CompletionModel + 'static,
{
    let (answer, usage) = forced_finish_turn(
        model,
        preamble,
        max_tokens,
        temperature,
        thinking,
        tools,
        chat_history,
        FINALIZE_NOTE,
    )
    .await
    .map_err(|e| {
        anyhow!(
            "model used all {max_turns} turns, and the forced final-answer turn \
             also failed to conclude: {e}"
        )
    })?;
    if answer.trim().is_empty() {
        return Err(empty_answer_error(
            model_name,
            max_turns,
            max_turns,
            &usage,
            log.last_finish_reason().as_deref(),
            "it used every turn and then wrote nothing when forced to conclude",
        ));
    }
    Ok((answer, usage))
}

/// One completion, no agent and no loop — the single-shot phases said plainly.
///
/// `oneshot` and `deliberate`'s direct lane are each *one* upstream request by
/// definition: the caller owns the context, there is nothing to explore, and there are
/// no tools to call. Running them through the managed tool loop with an empty toolset
/// arrived at the same place by a longer road; this asks the provider directly.
///
/// **Byte-identical to what the agent built.** rig's agent prepares its request with
/// the same [`CompletionRequestBuilder`](rig_core::completion::CompletionRequestBuilder)
/// this uses, and `build()` is what turns a preamble into the leading
/// `Message::System` — so preamble placement, params, `max_tokens`, temperature, the
/// empty tool list, and the absent `tool_choice` all land exactly as before. Using
/// rig's own builder rather than a hand-written literal is deliberate: a rig bump that
/// changes how a request is assembled moves both paths together.
///
/// The answer text is the assistant turn's text content concatenated, which is how
/// rig's runner builds `PromptResponse::output`; `usage` is the provider's report for
/// this one call, zero-valued when it reported none.
///
/// `log` records how the completion ended, the same seam [`run_phase_logged`] gives
/// the loop.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named request input
#[tracing::instrument(
    name = "run_phase",
    skip_all,
    fields(
        model = %model_name,
        // Honest, not decorative: this phase *is* one turn.
        max_turns = 1,
        gen_ai.request.thinking = tracing::field::Empty,
        gen_ai.response.finish_reason = tracing::field::Empty,
    )
)]
pub(crate) async fn run_completion<M>(
    model: &M,
    model_name: &str,
    log: &CompletionLog,
    preamble: &str,
    max_tokens: u64,
    temperature: Option<f64>,
    prompt: Message,
    thinking: Option<&Value>,
) -> Result<(String, Usage)>
where
    M: CompletionModel + 'static,
{
    if let Some(t) = thinking {
        tracing::Span::current().record("gen_ai.request.thinking", tracing::field::display(t));
    }
    let mut builder = watched(model.clone(), log.clone())
        .completion_request(prompt)
        .preamble(preamble.to_string())
        .max_tokens(max_tokens);
    if let Some(t) = temperature {
        builder = builder.temperature(t);
    }
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    // `send()` is `model.completion(builder.build())` — the one provider call.
    let response = builder.send().await;
    if let Some(reason) = log.last_finish_reason() {
        tracing::Span::current().record("gen_ai.response.finish_reason", reason.as_str());
    }
    // "model call failed" keeps this on the provider side of `classify_failure`
    // (`server/render.rs`), where a loop failure lands via "model loop failed" — an
    // overload or rate limit here is still worth a caller retry.
    let response = response.map_err(|e| anyhow!("model call failed: {e}"))?;
    let answer = response
        .choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    // The single-shot lanes are toolless by definition, so an empty answer here can
    // never satisfy the evidence gate the loop's recovery runs on — there is nothing
    // gathered to write up, and no re-ask that wouldn't invite an ungrounded answer.
    // Fail with the same diagnostics vocabulary as the loop, finish reason included
    // (this path reads it off its own log).
    if answer.trim().is_empty() {
        return Err(empty_answer_error(
            model_name,
            1,
            1,
            &response.usage,
            log.last_finish_reason().as_deref(),
            "the single toolless completion returned no answer text, and a lane with \
             no tools has no gathered evidence to write up, so kaibo does not re-ask",
        ));
    }
    Ok((answer, response.usage))
}

/// Run the explorer phase once and return its cited report. The explorer [`Arm`]
/// drives a fresh `{run_kaish}` toolset over a spawned kernel, bounded by
/// `max_turns`, and hands back the curated report the [`report_preamble`] shape
/// asks for. This is the one seam both callers of the explorer share: the nested
/// `explore′` sub-agent inside [`consult_with`] (via [`RunExplore::call`]) and the
/// top-level `explore` tool (via [`explore_with`]).
///
/// `preamble` is already resolved (the report shape + `[orientation]` + house
/// rules) — this fn is just the inner `arm.run`, so preamble composition and the
/// progress bracket stay with each caller. A fresh kernel per tool build ([`run_phase`]
/// may build a second for the turn-cap recovery turn); the shared `progress` sink is
/// handed to `run_kaish` so the sweep's own reads surface too. `!Send` care (an
/// invariant): the kernel stays on its `KaishWorker` thread and never crosses the
/// `.await`.
///
/// `attach` is the sweep's [`SweepAttachSink`] — `Some` injects the `attach` tool
/// alongside `run_kaish` (sharing the same kernel `view_image` shares with
/// `run_kaish` in `consult_tools`) and appends [`explorer_attach_directive`] to the
/// preamble in the same place, so preamble and toolset can never drift apart.
/// `None` (the top-level `explore` tool, v1) is byte-for-byte the old behavior —
/// same preamble, same two-line toolset.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named loop input
pub(crate) async fn run_explore_phase(
    arm: &Arm,
    preamble: &str,
    question: &str,
    root: PathBuf,
    sandbox: &SandboxConfig,
    max_turns: usize,
    progress: &Arc<dyn ProgressSink>,
    attach: Option<&Arc<SweepAttachSink>>,
) -> Result<(String, Usage)> {
    let preamble_owned;
    let preamble = match attach {
        Some(sink) => {
            preamble_owned = format!(
                "{preamble}{}",
                explorer_attach_directive(sink.max_attachments(), sink.consumer())
            );
            preamble_owned.as_str()
        }
        None => preamble,
    };
    arm.run(
        preamble,
        Message::user(question.to_string()),
        max_turns,
        progress.as_ref(),
        &|| -> Result<Vec<DynamicTool>> {
            let worker = KaishWorker::spawn_with(&root, sandbox.clone())?;
            let mut tools: Vec<DynamicTool> = vec![traced(RunKaish::with_progress(
                worker.clone(),
                progress.clone(),
            ))];
            if let Some(sink) = attach {
                tools.push(traced(SweepAttach::new(
                    worker,
                    root.clone(),
                    Arc::clone(sink),
                    progress.clone(),
                )));
            }
            Ok(tools)
        },
    )
    .await
}

/// `explore′` — the explorer unit wrapped as a rig [`Tool`] the consult loop can
/// call. Its `call` runs a *nested* agent: the explorer [`Arm`] (a cheap model,
/// possibly on a different backend than the driver) driving `{run_kaish}` over a
/// fresh kernel, returning a curated report. This is what lets the capable
/// `consult` model delegate a broad repo sweep instead of reading every span
/// itself.
///
/// `!Send` care (an invariant): the nested kernel stays on its `KaishWorker`
/// thread and never crosses the `.await` here — only the `Send` worker handle
/// does — so `call`'s future is `Send`, as rig requires. `tests/explore_send.rs`
/// pins this at compile time.
pub struct RunExplore {
    /// The explorer's resolved arm: its own client, model, params, `max_tokens`.
    arm: Arm,
    max_turns: usize,
    root: PathBuf,
    /// Sandbox limits for the fresh kernel each delegated sweep spawns.
    sandbox: SandboxConfig,
    /// Every delegated report is appended here, so the caller can surface what the
    /// sweeps found (the recomposed `consult`'s `report`) and a test can observe
    /// that a delegation actually happened.
    reports: Arc<Mutex<Vec<String>>>,
    /// Every sweep's token usage sums in here. The explorer runs *inside* the synth's
    /// tool loop, so its tokens never reach the synth's own `PromptResponse.usage` —
    /// this shared cell is how `consult_with` recovers them to fold into the consult
    /// total. Parallel to `reports`: one sink, many sweeps.
    usage_sink: Arc<Mutex<Usage>>,
    /// Liveness for the sweep: brackets each delegation with start/finish, and is
    /// handed to the nested kernel's `run_kaish` so the sub-agent's own reads show
    /// through too (a delegated sweep is where a long consult spends its silence).
    progress: Arc<dyn ProgressSink>,
    /// The sweep's fully-resolved system prompt, computed once by `consult_tools`:
    /// the explorer override-or-default with house rules already appended. So the
    /// nested explorer carries the explorer's `[prompts]`/`[context]` framing,
    /// built once instead of per sweep.
    preamble: Arc<str>,
    /// Who reads this sweep's report — decides the vision gate and the demotion
    /// wording a fresh [`SweepAttachSink`] is built with per sweep.
    consumer: SweepConsumer,
    /// This sweep's attach budget. `0` means "don't inject the tool at all" — no
    /// sink is built, `run_explore_phase` gets `None`, byte-for-byte the pre-attach
    /// behavior.
    max_attachments: usize,
    /// Canonical paths whose bytes already reach the consumer another way (the
    /// caller's own `consult` attachments) — seeded into every sweep's sink so
    /// `attach` doesn't re-route what's already in front of the driver. Empty for
    /// `deliberate` (see `SweepAttachSink`'s doc).
    already_delivered: Arc<HashSet<PathBuf>>,
}

impl RunExplore {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        arm: Arm,
        max_turns: usize,
        root: impl Into<PathBuf>,
        sandbox: SandboxConfig,
        reports: Arc<Mutex<Vec<String>>>,
        usage_sink: Arc<Mutex<Usage>>,
        progress: Arc<dyn ProgressSink>,
        preamble: Arc<str>,
        consumer: SweepConsumer,
        max_attachments: usize,
        already_delivered: Arc<HashSet<PathBuf>>,
    ) -> Self {
        Self {
            arm,
            max_turns,
            root: root.into(),
            sandbox,
            reports,
            usage_sink,
            progress,
            preamble,
            consumer,
            max_attachments,
            already_delivered,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RunExploreArgs {
    /// The question or sub-question to investigate across the repo.
    pub question: String,
}

/// The nested explore loop failed (the sub-agent errored or its worker died).
#[derive(Debug)]
pub struct RunExploreError(String);

impl std::fmt::Display for RunExploreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "explore failed: {}", self.0)
    }
}

impl std::error::Error for RunExploreError {}

impl Tool for RunExplore {
    const NAME: &'static str = "explore";
    type Error = RunExploreError;
    type Args = RunExploreArgs;
    /// [`ToolOutput`], not `String`: no attachment → `ToolOutput::text(report)`,
    /// exactly the plain report the driver always read. With a routed image → the
    /// text plus one *declared* `Image` block per routed image, the same typed
    /// contract [`crate::view_image::ViewImage::view`] emits — rig 0.41 only routes
    /// images a tool declares, never ones it might discover inside text output.
    type Output = ToolOutput;

    /// Keep the failure text model-visible: rig's default would redact it to a
    /// kind-level "the tool failed", and a dead sweep is something the driver must
    /// *see* to recover from — it answers from its own direct reads instead of retrying
    /// blind, and can only choose that if the failure reaches it. `with_source` keeps
    /// the concrete error for operator diagnostics and downcasting.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_source(error)
    }

    fn description(&self) -> String {
        "Delegate a broad sweep to the fast explorer on your team. It \
         searches the repository on a read-only kaish shell and reports back \
         with concrete `file:line` citations. Give it a focused question. Use \
         `explore` when a question needs breadth, and use `run_kaish` to read \
         specific code yourself."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "the question or sub-question to investigate"
                }
            },
            "required": ["question"]
        })
    }

    async fn call(
        &self,
        _ctx: &mut rig_agent::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        // Bracket the delegation: the start carries the sub-question, the finish fires
        // on both success and failure (the `?` below short-circuits, so emit it before
        // unwrapping the result).
        self.progress.emit(PhaseEvent::SweepStarted {
            question: args.question.clone(),
        });
        // A fresh sink per sweep — `attach`'s budget/dedupe is scoped to ONE sweep,
        // not the whole consult (a driver that delegates 5 sweeps pays for 5 budgets;
        // see the residual-risk note in the design doc). `0` means the operator
        // turned the tool off: no sink, `run_explore_phase` gets `None`, byte-for-byte
        // the pre-attach behavior.
        let sink = (self.max_attachments > 0).then(|| {
            Arc::new(SweepAttachSink::new(
                self.max_attachments,
                self.consumer.clone(),
                (*self.already_delivered).clone(),
            ))
        });
        // Reuse the one seam — explore′ is just the shared explorer phase, run on the
        // sub-agent's arm with its resolved preamble. The sweep bracket
        // (started/finished) and the reports-sink push stay here (consult-loop
        // specific); `run_explore_phase` is only the inner `arm.run`.
        let result = run_explore_phase(
            &self.arm,
            &self.preamble,
            &args.question,
            self.root.clone(),
            &self.sandbox,
            self.max_turns,
            &self.progress,
            sink.as_ref(),
        )
        .await;
        self.progress.emit(PhaseEvent::SweepFinished);
        let (report, usage) = result.map_err(|e| RunExploreError(format!("{e:#}")))?;
        // Fold this sweep's tokens into the shared consult total before returning the
        // report to the driver — the synth's own `PromptResponse` will never see them.
        // Lock poisoning means another delegation panicked — surface it, don't mask.
        *self
            .usage_sink
            .lock()
            .expect("explore usage sink poisoned") += usage;
        // A sweep that reported nothing is a *failed* sweep, not an empty one. Handing the
        // driver a blank tool result is the same silent-empty class one level down, and
        // worse in one way: the driver cannot tell a blank sweep from a sweep that found
        // nothing, so it would answer as though the breadth had been covered. As an error
        // it becomes visible — rig folds it back as this tool's result and the driver
        // recovers (re-delegate, or read the spans itself). Checked AFTER the usage fold,
        // because those tokens were spent either way and the footer must say so.
        // `run_phase`'s own guard makes this near-unreachable today; it stays as this
        // seam's invariant, not a duplicate of that one.
        if report.trim().is_empty() {
            return Err(RunExploreError(
                "the explorer returned an empty report — no findings reached the driver"
                    .to_string(),
            ));
        }
        // The `reports` sink feeds `ConsultOutput.report` (the readable artifact a
        // caller sees via `include_report`) — the explorer's own prose only, never
        // the routed bytes (those already reached the driver on the tool result; a
        // caller-facing summary duplicating full attached files would just bloat it).
        self.reports
            .lock()
            .expect("explore report sink poisoned")
            .push(report.clone());
        // What the DRIVER sees on the tool result: the report plus whatever this
        // sweep routed via `attach` — text bodies inline, an image manifest, notes,
        // and demotions. `None` (no sink, or a sink that never attached anything)
        // leaves this byte-for-byte the bare report, and `images` stays empty.
        let (text, images) = match &sink {
            Some(sink) => {
                let delivery = sink.drain();
                let images = delivery.images();
                let text = match sweep_evidence_block(&self.consumer, &delivery) {
                    Some(block) => format!("{report}{block}"),
                    None => report,
                };
                (text, images)
            }
            None => (report, Vec::new()),
        };
        // No image → plain text, exactly the pre-attach tool result (see the `Output`
        // doc). With one or more → the text followed by a declared `Image` block per
        // routed image, the same typed blocks `view_image` emits for one.
        if images.is_empty() {
            Ok(ToolOutput::text(text))
        } else {
            let mut parts: Vec<ToolResultContent> = vec![ToolResultContent::text(text)];
            parts.extend(images.iter().filter_map(|a| match a {
                Attachment::Image { mime, data_b64, .. } => Some(ToolResultContent::Image(Image {
                    data: DocumentSourceKind::Base64(data_b64.clone()),
                    media_type: ImageMediaType::from_mime_type(mime),
                    detail: None,
                    additional_params: None,
                })),
                Attachment::Text { .. } => None,
            }));
            Ok(ToolOutput::content(
                OneOrMany::many(parts).expect("the text part is always present"),
            ))
        }
    }
}

/// Assemble a single user turn from `text` plus any image attachments — shared by
/// [`oneshot`] and [`deliberate_direct`] (both are exactly-one-completion phases with
/// no tool loop to fold an image into, so the image must ride the initial turn
/// itself). No images → a bare `Message::user(text)`, byte-for-byte the pre-attach
/// call. With images, the text rides as the first part (skipped when empty — an
/// image-only turn shouldn't emit a pointless `{type:text,text:""}` block), then the
/// images. `&[]` is byte-for-byte the old `Message::user(text)` either way.
fn user_turn_with_attachments(attachments: &[Attachment], text: String) -> Message {
    let image_parts: Vec<UserContent> = attachments
        .iter()
        .filter_map(|a| match a {
            Attachment::Image { mime, data_b64, .. } => Some(UserContent::image_base64(
                data_b64.clone(),
                ImageMediaType::from_mime_type(mime),
                None,
            )),
            Attachment::Text { .. } => None,
        })
        .collect();
    if image_parts.is_empty() {
        Message::user(text)
    } else {
        let mut parts = Vec::with_capacity(image_parts.len() + 1);
        if !text.is_empty() {
            parts.push(UserContent::text(text));
        }
        parts.extend(image_parts);
        Message::User {
            content: OneOrMany::many(parts).expect("image_parts is non-empty on this branch"),
        }
    }
}

/// The `oneshot` seam: one direct completion on the resolved arm — no tools, no
/// shell, no exploration. The thin counterpart to `consult` (prompt in, answer out)
/// for when the caller already owns the context and just wants the model's take.
/// Exactly one upstream request, and now literally so: [`Arm::complete`] asks the
/// provider directly rather than routing a toolless single turn through the managed
/// loop. Neither orientation nor operator house rules are spliced — both are project
/// guidance, and oneshot reads no project.
///
/// Give oneshot a tool one day and this is the line to revisit: a direct completion
/// has nowhere to dispatch a tool call to, so a toolful oneshot belongs back on
/// [`run_phase`], not here.
///
/// `attachments` are caller-named workspace files inlined as context (the `attach` arg),
/// resolved server-side so their bytes never transit the calling agent's context — the
/// same seam batch uses. Text files prepend to the prompt as `<file>`-wrapped context
/// (`attach::with_text_context`); images ride beside the prompt as native rig image parts
/// on the single user turn. The image caller must already have gated on the model's
/// vision cap (the server does, before this runs). With no attachments this is exactly
/// the bare prompt and an empty part list — byte-for-byte the old single call.
pub async fn oneshot(
    prompt: &str,
    attachments: &[Attachment],
    arm: &Arm,
    cfg: &PhaseContext,
) -> Result<(String, Usage)> {
    let user_prompt = crate::attach::with_text_context(attachments, prompt);
    let initial_prompt = user_turn_with_attachments(attachments, user_prompt);
    with_call_deadline(
        cfg.call_deadline,
        "oneshot",
        arm.complete(
            &resolve_phase_preamble(
                Phase::Oneshot,
                &cfg.prompts,
                cfg.orientation.as_deref(),
                cfg.house_rules.as_deref(),
            ),
            initial_prompt,
        ),
    )
    .await
}

/// Build the recomposed `consult` toolset: `{run_kaish, explore′}`. Factored out so
/// the wiring (both tools present, explore′ pointed at the explorer arm) is
/// unit-testable without a live model. `reports` collects each `explore′` sweep.
#[allow(clippy::too_many_arguments)] // the sinks and flags are each a distinct wiring input
fn consult_tools(
    explorer: &Arm,
    root: &Path,
    cfg: &ConsultConfig,
    reports: Arc<Mutex<Vec<String>>>,
    explore_usage: Arc<Mutex<Usage>>,
    synth: &Arm,
) -> Result<Vec<DynamicTool>> {
    // run_kaish for precise reads by the consult model itself — carries the sink so
    // the driver's own reads show up as progress alongside the delegated sweeps'.
    let worker = KaishWorker::spawn_with(root, cfg.explore.sandbox.clone())?;
    // explore′ for delegated breadth: the same explore unit, wrapped as a tool,
    // pointed at the explorer arm — its own client, model, and request shape,
    // which may live on a different backend than the driver's. Bounded by
    // explorer_max_turns per sweep; no cap on how many times consult may delegate
    // (Amy's call — watch real behavior). Its system prompt is the explorer
    // override-or-default + house rules, built once here rather than per sweep —
    // plus the caller's attachment directive: a sweep is a fresh agent that saw
    // neither the driver prompt nor the inlined bytes, so without this a driver
    // that delegates early sends a sweep blind to the very files the caller
    // flagged as central. The directive orders whole `cat -n` reads (the explorer
    // chooses when, not whether), keeping citation-exact line numbers for free.
    let mut explorer_preamble_owned = resolve_phase_preamble(
        Phase::Explorer,
        &cfg.explore.phase.prompts,
        cfg.explore.phase.orientation.as_deref(),
        cfg.explore.phase.house_rules.as_deref(),
    );
    if let Some(directive) = super::prompts::explorer_attachment_directive(&cfg.attachments) {
        explorer_preamble_owned.push_str(&directive);
    }
    let explorer_preamble: Arc<str> = Arc::from(explorer_preamble_owned);
    // Who a delegated sweep's `attach` calls route to: the DRIVER (this loop runs on
    // the synth arm), named so the explorer knows who reads its report, gated on the
    // synth's own vision cap (the same cap `view_image` below rides).
    let consumer = SweepConsumer {
        kind: SweepConsumerKind::ConsultDriver,
        label: Arc::from(format!("the consult driver (`{}`)", synth.model)),
        vision: synth.caps.vision,
    };
    // The caller's own `consult` attachments already reach the driver another way
    // (inlined, or a read-WHOLE directive) — seed the sink so a sweep's `attach`
    // doesn't re-route what's already in front of the driver.
    // Resolved exactly as `attach_one` resolves an explorer's path — canonicalized, not
    // merely joined. A plain join misses the dedupe for any caller path carrying a `./`,
    // a `..`, or a symlink, and the reader then receives the same bytes twice.
    let already_delivered: HashSet<PathBuf> =
        crate::sweep_attach::delivered_seed(root, cfg.attachments.iter().map(|a| Path::new(a.path())));
    let explore = RunExplore::new(
        explorer.clone(),
        cfg.explore.explorer_max_turns,
        root,
        cfg.explore.sandbox.clone(),
        reports,
        explore_usage,
        cfg.explore.phase.progress.clone(),
        explorer_preamble,
        consumer,
        cfg.explore.max_attachments,
        Arc::new(already_delivered),
    );
    let mut tools: Vec<DynamicTool> = vec![
        traced(RunKaish::with_progress(
            worker.clone(),
            cfg.explore.phase.progress.clone(),
        )),
        traced(explore),
    ];
    // The driver loop runs on the *synth* arm, so view_image rides the synth's
    // vision cap (the delegated explore′ sub-agent gets its own view_image keyed to
    // the explorer arm's caps, inside `explore`). Shares the driver's kernel.
    if synth.caps.vision {
        tools.push(traced(ViewImage::new(worker, root.to_path_buf())));
    }
    // save_artifact, when the server built a sink for this call (all three keys held:
    // operator config, the caller's `save_artifacts`, a live media CAS). The DRIVER only
    // — a delegated sweep builds its toolset in `run_explore_phase` from the explore
    // rung, which carries no sink, so this cannot reach one. Every rebuild of this
    // toolset (the main loop, the forced final turn) shares the one sink, so the
    // per-call caps count the call rather than the turn.
    if let Some(sink) = &cfg.artifacts {
        tools.push(traced(SaveArtifact::new(Arc::clone(sink))));
    }
    Ok(tools)
}

/// Run the `explore` tool: the evidence-gathering half of `consult`, surfaced on
/// its own. One explorer arm sweeps `root` read-only over `{run_kaish}` and returns
/// its cited report *verbatim* — no synth, no session. `attached` files (the tool's
/// `attach` arg; deliberate's dossier stage passes none) land as a preamble
/// directive to read each WHOLE with `cat -n` — the explorer reads through its
/// shell, so nothing is inlined here. The report shape is [`report_preamble`],
/// resolved through the same [`phase_preamble`] layering `consult_tools` gives the
/// nested `explore′`, so a `[prompts].explorer` override, `[orientation]`, and
/// house rules all reach it.
pub(crate) async fn explore_with(
    question: &str,
    root: PathBuf,
    explorer: &Arm,
    cfg: &ExploreConfig,
    attached: &[super::prompts::ConsultAttachment],
    attach: Option<&Arc<SweepAttachSink>>,
) -> Result<(String, Usage)> {
    let mut preamble = resolve_phase_preamble(
        Phase::Explorer,
        &cfg.phase.prompts,
        cfg.phase.orientation.as_deref(),
        cfg.phase.house_rules.as_deref(),
    );
    if let Some(directive) = super::prompts::explorer_attachment_directive(attached) {
        preamble.push_str(&directive);
    }
    with_call_deadline(
        cfg.phase.call_deadline,
        "explore",
        run_explore_phase(
            explorer,
            &preamble,
            question,
            root,
            &cfg.sandbox,
            cfg.explorer_max_turns,
            &cfg.phase.progress,
            attach,
        ),
    )
    .await
}

/// Run one call's model work under a wall-clock ceiling.
///
/// The per-request `request_timeout` (down in reqwest, injected through rig) is the
/// *first* brake on a stalled backend; this is the transport-agnostic backstop for
/// when it doesn't fire — a wedged local server holding a pooled keep-alive, rig's
/// split send-then-body read. On elapse the call aborts loudly instead of hanging the
/// caller's session indefinitely (the 2026-07-02 ~17h park a stopped local backend
/// caused). The interactive loop tools pass `call_deadline` here (`consult`/`explore`/
/// `oneshot` and the async `consult_submit`); `deliberate`'s direct lane passes a
/// deadline sized to its synth backend's `request_timeout` instead (one completion, so
/// `request_timeout` is its natural bound). The batch lane calls this not at all — kaibo
/// holds no wait there, the deliberation runs on the provider's queue.
async fn with_call_deadline<T>(
    deadline: Duration,
    label: &str,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(deadline, fut).await {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!(
            "{label} exceeded its {}s wall-clock deadline — a backend or model stopped \
             responding. Raise `call_deadline_secs` (or `KAIBO_CALL_DEADLINE_SECS`) if this \
             was a legitimately long run.",
            deadline.as_secs()
        )),
    }
}

/// Run `deliberate`'s offline synth on the **direct** lane: one long, toolless local
/// completion over the dossier. Same shape as [`oneshot`] (empty toolset, a single
/// turn — exactly one upstream request) but on the offline-synth preamble and framing
/// the dossier as trusted evidence. The arm points at a big local model whose backend
/// `request_timeout` may stretch long; kaibo holds the one completion open in a
/// background job. `system` is the resolved offline-synth preamble (shared with batch
/// via [`batch_system_prompt`]).
///
/// It's async (the caller collects a `job-N` handle, never blocks) but *not* unbounded:
/// this is an in-process completion kaibo holds, so it wears a `call_deadline`-style
/// wall-clock backstop — a wedged local server can't leave a job running forever, and
/// `job_wait`/`job_get` resolve within the deadline. Because it's exactly *one*
/// completion (unlike a multi-turn `consult` loop), the caller sizes `deadline` to the
/// synth backend's own `request_timeout` (+ a margin), not the interactive-loop
/// `call_deadline` — a slow local model gets its full patience without forcing the
/// interactive ceiling high. The **batch** lane, by contrast, holds no in-process wait
/// at all — its deliberation runs on the *provider's* queue.
///
/// `attachments` carries any images a delegated dossier sweep routed via `attach`
/// (text attachments never reach here — they're already stitched into `dossier`'s
/// text by `sweep_evidence_block`) — the single turn's own images, via the same
/// [`user_turn_with_attachments`] helper [`oneshot`] uses. `&[]` is byte-for-byte the
/// old `Message::user(...)` call.
#[allow(clippy::too_many_arguments)] // each arg is a distinct, named phase input
pub async fn deliberate_direct(
    question: &str,
    dossier: &str,
    attachments: &[Attachment],
    synth: &Arm,
    system: &str,
    deadline: Duration,
) -> Result<(String, Usage)> {
    let prompt = user_turn_with_attachments(attachments, deliberation_prompt(question, dossier));
    with_call_deadline(deadline, "deliberate", synth.complete(system, prompt)).await
}

/// Run a `consult` over two resolved arms.
///
/// One loop, two tools — no rigid explorer→synth hand-off. The synthesis agent
/// decides when to delegate a sweep to the cheap `explore′` vs. read a span
/// directly with `run_kaish`. Each arm carries its own client and request shape,
/// so a mixed cast routes each phase to its own backend through the same loop.
/// `ConsultOutput.report` aggregates whatever the `explore′` sweeps returned
/// (empty if the model read everything itself).
pub(crate) async fn consult_with(
    user_prompt: &str,
    root: &Path,
    explorer: &Arm,
    synth: &Arm,
    cfg: &ConsultConfig,
) -> Result<ConsultOutput> {
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    // The delegated sweeps' tokens land here (they run inside the synth loop, so the
    // synth's own `PromptResponse.usage` never counts them); summed with the synth's
    // usage below into the consult total.
    let explore_usage = Arc::new(Mutex::new(Usage::new()));

    let (answer, synth_usage) = with_call_deadline(
        cfg.explore.phase.call_deadline,
        "consult",
        synth.run(
            &resolve_phase_preamble(
                Phase::Consult,
                &cfg.explore.phase.prompts,
                cfg.explore.phase.orientation.as_deref(),
                cfg.explore.phase.house_rules.as_deref(),
            ),
            Message::user(user_prompt.to_string()),
            cfg.synth_max_turns,
            cfg.explore.phase.progress.as_ref(),
            // Rebuilt per call (main loop, and again if run_phase forces a final
            // turn); every build shares the one `reports` + `explore_usage` sink so
            // all explore′ sweeps aggregate.
            &|| {
                consult_tools(
                    explorer,
                    root,
                    cfg,
                    reports.clone(),
                    explore_usage.clone(),
                    synth,
                )
            },
        ),
    )
    .await
    .context("consult loop")?;

    let report = reports
        .lock()
        .expect("explore report sink poisoned")
        .join("\n\n---\n\n");
    // Consult total: the synth's own loop plus every delegated sweep.
    let usage = synth_usage + *explore_usage.lock().expect("explore usage sink poisoned");
    Ok(ConsultOutput {
        answer,
        report,
        usage,
        // Populated downstream (`consult_session_turn` on a record failure); the raw
        // consult loop has no non-fatal notices of its own.
        warnings: Vec::new(),
    })
}

/// A consult turn's session binding: the session backend and the session id. `None` is a
/// stateless one-shot — no prior turns replayed, nothing recorded. `Some` makes it
/// multi-turn: replay this session's history into the prompt, record the answer. The
/// backend ([`Sessions`]) is in-memory or durable; this glue doesn't care which.
pub type Session<'a> = (&'a Sessions, &'a str);

/// One sessioned (or stateless) consult turn over two resolved arms.
///
/// This is the whole multi-turn glue, driven offline by scripted arms in tests
/// (the public [`consult`] is a thin named wrapper): read the session's prior
/// turns → frame the prompt with them → run the consult → record the answer. The
/// exploration always runs fresh; only the lean `(question, answer)` pairs are
/// replayed. Recording happens *after* a successful turn (`?` short-circuits a
/// failure), so a failed consult never poisons the thread with a half-answer the
/// next turn would treat as established context.
pub(crate) async fn consult_session_turn(
    session: Option<Session<'_>>,
    question: &str,
    context: Option<&str>,
    root: &Path,
    explorer: &Arm,
    synth: &Arm,
    cfg: &ConsultConfig,
) -> Result<ConsultOutput> {
    // History (replay) failure is FATAL: nothing is paid for yet, so a broken store should
    // fail the turn loudly rather than silently answer without the thread's context.
    let history = match session {
        Some((store, id)) => store.history(id).await?,
        None => Vec::new(),
    };
    let user_prompt = consult_user_prompt(question, context, &history, &cfg.attachments);

    let mut out = consult_with(&user_prompt, root, explorer, synth, cfg).await?;

    // Record failure is NON-fatal: by here the model has answered (a paid-for result), so
    // losing that answer over a bookkeeping write would be the wrong trade — and it mirrors
    // the non-fatal batch-handle record. Instead, keep the answer and surface the failure
    // LOUDLY (never a silent fallback): warn on the log, and record a caller-visible notice
    // on `out.warnings` — kept OFF the answer text so a machine consumer of the answer gets
    // the model's words uncorrupted (each front door renders warnings its own way). The
    // in-memory arm never errors, so today's default is byte-for-byte unchanged.
    if let Some((store, id)) = session {
        // Record the answer WITH this call's artifact footer, not the bare answer. The
        // digests are the only handle on bytes that outlive the call, and kaibo prunes
        // nothing, so they need somewhere durable — and the session turn already sits in
        // the state db beside the conversation that produced them, at no schema cost. A
        // later turn replaying this thread then sees what it saved, which is the honest
        // continuity too: the model asked for the artifact, so the thread should remember
        // its address. This is the *persisted* view; the client's copy is rendered by the
        // server, where warnings ride between the answer and the footer.
        let recorded =
            crate::artifact::with_artifacts(out.answer.clone(), cfg.artifacts.as_deref());
        if let Err(e) = store.record(id, QaTurn::new(question, recorded)).await {
            tracing::warn!(
                session = id,
                error = %e,
                "session turn not recorded — the answer stands, but this thread won't replay it"
            );
            out.warnings.push(format!(
                "⚠️ Session turn NOT recorded (persistence error: {e}). \
                 The answer is complete, but this `session_id` won't include it on your \
                 next turn — retry, or continue without relying on this turn as context."
            ));
        }
    }
    Ok(out)
}

/// Run a consult against `root` over the resolved `explorer` and `synth` arms.
///
/// The server resolves a cast's slots into the arms ([`Arm::from_slot`] — keys,
/// endpoints, request shapes); `cfg` carries the per-call loop bounds. `session`
/// binds this turn to a multi-turn thread (replay prior turns, record this one) or is
/// `None` for a stateless one-shot. The session seeds the driver's prompt but never
/// the exploration, which always runs fresh. See [`consult_session_turn`].
pub async fn consult(
    question: &str,
    context: Option<&str>,
    root: impl Into<PathBuf>,
    explorer: &Arm,
    synth: &Arm,
    cfg: &ConsultConfig,
    session: Option<Session<'_>>,
) -> Result<ConsultOutput> {
    let root = root.into();
    consult_session_turn(session, question, context, &root, explorer, synth, cfg).await
}

#[cfg(test)]
mod tests {
    use super::super::shaping::thinking_params;
    use super::*;
    use std::time::Duration;

    use crate::credentials::ProviderKind;
    use crate::session::{SessionStore, Sessions};
    use crate::test_support::{
        has_tool, is_finalize_turn, provider_error, reasoning_response, text_response,
        tool_call_response, transcript_text, usage, with_raw, with_usage, CaptureHttp,
        RecordingSink, ScriptedClient,
    };
    use rig_core::completion::CompletionRequest;
    use std::fs;
    use std::num::NonZeroUsize;
    use std::path::Path;
    use tempfile::tempdir;

    /// The in-memory session backend behind the seam — today's default.
    fn store() -> Sessions {
        Sessions::Memory(SessionStore::new(NonZeroUsize::new(4).unwrap()))
    }

    /// The durable, turso-backed session backend at a tempfile db — the persistence path.
    /// Same seam the server threads through `consult_session_turn`, so a store-backed
    /// replay exercises exactly the wiring a persistent server runs.
    async fn persistent_store(dir: &Path) -> Sessions {
        let path = dir.join("state.db");
        Sessions::Persistent(
            crate::store::SessionStore::open(&path, NonZeroUsize::new(4).unwrap(), &[])
                .await
                .expect("open persistent store"),
        )
    }

    /// An arm over the scripted client with no thinking params — for the tests
    /// that exercise the loop wiring (report aggregation, sessions, turn caps)
    /// and don't care about request shaping.
    fn arm(client: &ScriptedClient, model: &str) -> Arm {
        arm_with(client, model, None)
    }

    /// An arm carrying explicit `additional_params` — the request-shaping tests'
    /// injection point (each arm gets params fit to *its* model, as the server's
    /// `Arm::from_slot` would resolve them).
    fn arm_with(client: &ScriptedClient, model: &str, params: Option<Value>) -> Arm {
        Arm::new(
            client.clone(),
            model,
            16384,
            params,
            ModelCaps {
                vision: false,
                tool_result_images: true,
            },
        )
    }

    /// A vision-capable arm on a transport that carries an image in a tool result
    /// (Anthropic/Gemini-shaped caps) — the toolset-wiring tests' injection point for
    /// "the synth can see", distinct from `arm`'s deliberately blind default.
    fn vision_arm(client: &ScriptedClient, model: &str) -> Arm {
        Arm::new(
            client.clone(),
            model,
            16384,
            None,
            ModelCaps {
                vision: true,
                tool_result_images: true,
            },
        )
    }

    /// A driver that answers immediately (no tools), echoing the current question into
    /// its answer so a later turn's replayed history is easy to spot. Keeps the
    /// session tests focused on the glue, not the loop. `consult_user_prompt` puts the
    /// current question last, so the final non-empty line is it.
    fn echo_client(model: &str) -> ScriptedClient {
        ScriptedClient::builder()
            .on_model(model, |req| {
                let shown = transcript_text(req);
                let question = shown
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                Ok(text_response(format!("ANSWER[{question}]")))
            })
            .build()
    }

    /// True for a recorded request that is a forced final turn (`ToolChoice::None`) —
    /// the observable signature of a turn-cap or empty-answer recovery. Taken by
    /// reference so it drops straight into `.filter(..)` over recorded requests.
    fn is_finalize_request(r: &crate::test_support::RecordedRequest) -> bool {
        r.tool_choice == Some(ToolChoice::None)
    }

    /// A project root with one real file carrying a known marker, so the kaish reads
    /// in the e2e below hit real bytes — not a stub.
    fn project_with_marker() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/foo.rs"), "fn target_marker() {}\n").unwrap();
        dir
    }

    /// The load-bearing wiring test for the house-rules feature: operator context
    /// configured on `ConsultConfig` must reach the *model's preamble*, and only
    /// when configured. A scripted phase answers immediately; we read back the
    /// role framing rig forwarded. The `None` arm proves the splice is gated, not
    /// unconditional — a server with no `[context]` runs the unchanged preamble.
    #[tokio::test]
    async fn house_rules_splice_into_the_phase_preamble_only_when_configured() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const MARKER: &str = "HOUSE_RULE_MARKER: prefer tabs over spaces";

        let dir = tempdir().unwrap();

        // Configured → the marker and its framing ride in the consult driver's
        // preamble. The synth answers immediately (no sweep needed for this check).
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |_req| Ok(text_response("done")))
            .build();
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    house_rules: Some(Arc::from(MARKER)),
                    ..PhaseContext::default()
                },
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };
        consult_with(
            "q",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();
        let reqs = client.requests_for(SYNTH);
        let pre = reqs[0].preamble.as_deref().unwrap_or("");
        assert!(
            pre.contains(MARKER),
            "house rules must reach the preamble: {pre}"
        );
        assert!(
            pre.contains("Operator house rules"),
            "the framing header must introduce them: {pre}"
        );
        // Still the consult driver's own role framing — house rules append, not replace.
        assert!(
            pre.contains("You are the synthesis agent"),
            "base preamble must remain: {pre}"
        );

        // Unconfigured → the same call carries the base preamble and no marker.
        let bare = ScriptedClient::builder()
            .on_model(SYNTH, |_req| Ok(text_response("done")))
            .build();
        let cfg2 = ConsultConfig::default(); // house_rules: None
        consult_with(
            "q",
            dir.path(),
            &arm(&bare, EXPLORER),
            &arm(&bare, SYNTH),
            &cfg2,
        )
        .await
        .unwrap();
        let reqs2 = bare.requests_for(SYNTH);
        let pre2 = reqs2[0].preamble.as_deref().unwrap_or("");
        assert!(!pre2.contains(MARKER), "no [context] → no marker: {pre2}");
        assert!(
            pre2.contains("You are the synthesis agent"),
            "base preamble intact: {pre2}"
        );
    }

    /// House rules reach the *nested* `explore′` sweep too, not just the driver —
    /// the consistency that lets the cheap explorer orient on `AGENTS.md` while it
    /// searches. Drives the real consult loop: the driver delegates one sweep, the
    /// explorer reports, the driver answers. We then assert BOTH models saw the
    /// marker in their preamble — the explorer via the `RunExplore`-threaded block.
    #[tokio::test]
    async fn house_rules_reach_the_nested_explorer_sweep() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const MARKER: &str = "HOUSE_RULE_MARKER: the cast lives in config.rs";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let seen = transcript_text(req);
                if !seen.contains("SWEEP_DONE") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "where does the cast live?" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: config.rs"))
                }
            })
            // The explorer answers its sweep immediately (no kaish needed for this
            // test — we only care what preamble it was handed).
            .on_model(EXPLORER, |_req| Ok(text_response("SWEEP_DONE")))
            .build();

        let dir = tempdir().unwrap();
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    house_rules: Some(Arc::from(MARKER)),
                    ..PhaseContext::default()
                },
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };
        consult_with(
            "where does the cast live?",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        // The driver saw it (as the standalone test also proves)...
        let synth_pre = client.requests_for(SYNTH)[0]
            .preamble
            .clone()
            .unwrap_or_default();
        assert!(synth_pre.contains(MARKER), "driver preamble: {synth_pre}");
        // ...and so did the nested explorer — the teeth for this change.
        let explorer_reqs = client.requests_for(EXPLORER);
        assert!(!explorer_reqs.is_empty(), "the sweep must have run");
        let explorer_pre = explorer_reqs[0].preamble.clone().unwrap_or_default();
        assert!(
            explorer_pre.contains(MARKER),
            "the nested explore′ sweep must carry the house rules too: {explorer_pre}"
        );
        assert!(
            explorer_pre.contains("You are the explorer"),
            "still the explorer's own role framing: {explorer_pre}"
        );
    }

    /// A `[prompts]` override **fully replaces** the built-in preamble: the
    /// operator's prose is verbatim, and the built-in role framing is *gone* (the
    /// kaish contract still rides the `run_kaish` tool, untested here). House rules
    /// still append on top — `[prompts]` and `[context]` are orthogonal axes. Driven
    /// on the `consult` driver, the phase that carries both an override and house
    /// rules (oneshot reads no project, so it carries neither house rules nor a map).
    #[tokio::test]
    async fn a_prompt_override_fully_replaces_the_preamble_and_house_rules_still_append() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const CUSTOM: &str = "You are a SECURITY AUDITOR. Hunt injection sinks.";
        const HOUSE: &str = "HOUSE_RULE_MARKER: prefer tabs";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |_req| Ok(text_response("done")))
            .build();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    prompts: PromptOverrides {
                        consult: Some(CUSTOM.to_string()),
                        ..PromptOverrides::default()
                    },
                    house_rules: Some(Arc::from(HOUSE)),
                    ..PhaseContext::default()
                },
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };
        consult_with(
            "q",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        let reqs = client.requests_for(SYNTH);
        let pre = reqs[0].preamble.as_deref().unwrap_or("");
        // The override is verbatim...
        assert!(pre.contains(CUSTOM), "override prose missing: {pre}");
        // ...the built-in framing is fully replaced (full-replace, by decision)...
        assert!(
            !pre.contains("You are the synthesis agent"),
            "override must REPLACE, not augment, the built-in: {pre}"
        );
        // ...and house rules still layer on top.
        assert!(pre.contains(HOUSE), "house rules must still append: {pre}");
    }

    /// Each phase reads its *own* override key — an `explorer`-only override must
    /// not bleed into the `oneshot` phase, which keeps its built-in. Guards the
    /// per-phase routing in [`phase_preamble`]/[`PromptOverrides`].
    #[tokio::test]
    async fn prompt_overrides_are_per_phase() {
        const MODEL: &str = "synth";
        const CUSTOM_EXPLORER: &str = "EXPLORER_ONLY_OVERRIDE";

        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        // Only the explorer key is set; the oneshot phase must ignore it.
        let cfg = PhaseContext {
            prompts: PromptOverrides {
                explorer: Some(CUSTOM_EXPLORER.to_string()),
                ..PromptOverrides::default()
            },
            ..PhaseContext::default()
        };
        oneshot("q", &[], &arm(&client, MODEL), &cfg).await.unwrap();

        let pre = client.requests_for(MODEL)[0]
            .preamble
            .clone()
            .unwrap_or_default();
        assert!(
            !pre.contains(CUSTOM_EXPLORER),
            "the explorer override must not bleed into oneshot: {pre}"
        );
        // oneshot keeps its built-in framing.
        assert!(
            pre.contains("second opinion"),
            "oneshot keeps its built-in preamble: {pre}"
        );
    }

    /// `oneshot` is the *thin* path: it must hand the model NO tools — no `run_kaish`,
    /// no `explore`, no `view_image`. The caller owns the context; there is no
    /// codebase access. Pin the empty toolset so a regression that wires a shell back
    /// in (re-collapsing oneshot into consult) fails here.
    #[tokio::test]
    async fn oneshot_offers_the_model_no_tools() {
        const MODEL: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        oneshot(
            "just answer this",
            &[],
            &arm(&client, MODEL),
            &PhaseContext::default(),
        )
        .await
        .unwrap();
        let reqs = client.requests_for(MODEL);
        assert_eq!(reqs.len(), 1, "oneshot is exactly one upstream request");
        assert!(
            reqs[0].tool_names.is_empty(),
            "oneshot must offer no tools, got {:?}",
            reqs[0].tool_names
        );
    }

    /// `oneshot` inlines its attachments onto the single user turn: a text file prepends
    /// as `<file>`-wrapped context ahead of the prompt, and an image rides as a native
    /// rig image part on the same message — the toolless analogue of batch's attach,
    /// driven through the real loop offline.
    #[tokio::test]
    async fn oneshot_inlines_text_and_image_attachments() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        const MODEL: &str = "synth";

        // The mock inspects the inbound request for an image part — the only way to
        // assert the image rode as a structured part (the text capture flattens parts).
        let saw_image = Arc::new(AtomicBool::new(false));
        let flag = saw_image.clone();
        let client = ScriptedClient::builder()
            .on_model(MODEL, move |req| {
                let has_image = req.chat_history.iter().any(|m| match m {
                    Message::User { content } => {
                        content.iter().any(|c| matches!(c, UserContent::Image(_)))
                    }
                    _ => false,
                });
                if has_image {
                    flag.store(true, Ordering::SeqCst);
                }
                Ok(text_response("done"))
            })
            .build();

        let attachments = vec![
            Attachment::Text {
                path: "README.md".into(),
                body: "hello world".into(),
            },
            Attachment::Image {
                path: "shot.png".into(),
                mime: "image/png",
                data_b64: "QUJD".into(),
            },
        ];
        oneshot(
            "review these",
            &attachments,
            &arm(&client, MODEL),
            &PhaseContext::default(),
        )
        .await
        .unwrap();

        let reqs = client.requests_for(MODEL);
        assert_eq!(reqs.len(), 1, "oneshot is exactly one upstream request");
        // The text file rode inline as `<file>`-wrapped context, ahead of the prompt.
        let ut = &reqs[0].user_text;
        assert!(
            ut.contains("<file path=\"README.md\">"),
            "text file inlined as context: {ut}"
        );
        assert!(
            ut.contains("hello world"),
            "the file body rode inline: {ut}"
        );
        assert!(
            ut.contains("review these"),
            "the prompt is still present: {ut}"
        );
        // The image rode as a native image part, not flattened into text.
        assert!(
            saw_image.load(Ordering::SeqCst),
            "the image attachment must ride as a structured image part"
        );
    }

    /// With no attachments, `oneshot`'s user turn is exactly the bare prompt and carries
    /// no image part — the no-attachment path stays byte-for-byte the old single call.
    #[tokio::test]
    async fn oneshot_without_attachments_is_the_bare_prompt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        const MODEL: &str = "synth";
        let saw_image = Arc::new(AtomicBool::new(false));
        let flag = saw_image.clone();
        let client = ScriptedClient::builder()
            .on_model(MODEL, move |req| {
                let has_image = req.chat_history.iter().any(|m| match m {
                    Message::User { content } => {
                        content.iter().any(|c| matches!(c, UserContent::Image(_)))
                    }
                    _ => false,
                });
                if has_image {
                    flag.store(true, Ordering::SeqCst);
                }
                Ok(text_response("done"))
            })
            .build();
        oneshot(
            "just ask",
            &[],
            &arm(&client, MODEL),
            &PhaseContext::default(),
        )
        .await
        .unwrap();
        let reqs = client.requests_for(MODEL);
        assert_eq!(
            reqs[0].user_text.trim(),
            "just ask",
            "bare prompt, no wrapper"
        );
        assert!(
            !saw_image.load(Ordering::SeqCst),
            "no attachment, no image part"
        );
    }

    /// An image-only attach with an empty prompt sends *just* the image part — no empty
    /// `{type:text, text:""}` chunk. Without the guard the user turn would carry a useless
    /// empty text part, so this counts text parts and pins it at zero.
    #[tokio::test]
    async fn oneshot_empty_prompt_image_only_omits_empty_text_part() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        const MODEL: &str = "synth";
        let text_parts = Arc::new(AtomicUsize::new(usize::MAX));
        let counter = text_parts.clone();
        let client = ScriptedClient::builder()
            .on_model(MODEL, move |req| {
                let n = req
                    .chat_history
                    .iter()
                    .find_map(|m| match m {
                        Message::User { content } => Some(
                            content
                                .iter()
                                .filter(|c| matches!(c, UserContent::Text(_)))
                                .count(),
                        ),
                        _ => None,
                    })
                    .unwrap_or(0);
                counter.store(n, Ordering::SeqCst);
                Ok(text_response("done"))
            })
            .build();
        let img = Attachment::Image {
            path: "x.png".into(),
            mime: "image/png",
            data_b64: "QUJD".into(),
        };
        oneshot("", &[img], &arm(&client, MODEL), &PhaseContext::default())
            .await
            .unwrap();
        assert_eq!(
            text_parts.load(Ordering::SeqCst),
            0,
            "an empty prompt must not add an empty text part beside the image"
        );
    }

    /// `oneshot` is now one *literal* upstream request — a direct completion, no agent
    /// and no tool loop — and it must ask for exactly what the loop used to ask for.
    ///
    /// The proof is a side-by-side: the same preamble, params, `max_tokens`, and prompt
    /// go down the old road (`Arm::run` with an empty toolset and a single turn) and the
    /// new one (`oneshot` → `Arm::complete`), on two scripted models sharing one request
    /// log. The two serialized [`CompletionRequest`]s must be identical — every field,
    /// including the ones no accessor names: preamble placement (rig turns it into the
    /// leading `System` message), the empty `tools`/`documents`, the absent
    /// `tool_choice`, `additional_params`. Comparing whole requests is the point: a
    /// spot-check of a few fields would pass while a dropped one changed the call.
    #[tokio::test]
    async fn oneshot_asks_exactly_what_the_agent_loop_asked() {
        const VIA_LOOP: &str = "via-agent-loop";
        const VIA_DIRECT: &str = "via-direct-call";
        let client = ScriptedClient::builder()
            .on_model(VIA_LOOP, |_req| Ok(text_response("done")))
            .on_model(VIA_DIRECT, |_req| Ok(text_response("done")))
            .build();
        // A real params blob, so the thinking toggle rides both paths.
        let params = Some(json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}));
        let cfg = PhaseContext::default();
        let preamble = resolve_phase_preamble(
            Phase::Oneshot,
            &cfg.prompts,
            cfg.orientation.as_deref(),
            cfg.house_rules.as_deref(),
        );

        // The pre-change path, reproduced: the managed loop, one turn, no tools.
        let (loop_answer, loop_usage) = arm_with(&client, VIA_LOOP, params.clone())
            .run(
                &preamble,
                Message::user("what changed?"),
                1,
                cfg.progress.as_ref(),
                &|| Ok(Vec::new()),
            )
            .await
            .expect("the agent-loop path answers");

        // The shipped path.
        let (direct_answer, direct_usage) = oneshot(
            "what changed?",
            &[],
            &arm_with(&client, VIA_DIRECT, params),
            &cfg,
        )
        .await
        .expect("the direct path answers");

        assert_eq!(direct_answer, loop_answer, "same answer text");
        assert_eq!(direct_usage, loop_usage, "same usage accounting");

        let looped = client.requests_for(VIA_LOOP);
        let direct = client.requests_for(VIA_DIRECT);
        assert_eq!(
            looped.len(),
            1,
            "the loop made one request to compare against"
        );
        assert_eq!(
            direct.len(),
            1,
            "oneshot is exactly ONE upstream request, got {}",
            direct.len()
        );
        assert_eq!(
            direct[0].raw, looped[0].raw,
            "the direct completion must be request-for-request what the agent built"
        );
    }

    /// The whole reason the wrapper exists: a phase's completion record reaches
    /// [`run_phase_logged`]'s caller **including the turns inside the tool loop** — the
    /// ones rig's agent hook can only see in its medium-neutral form, with no
    /// `raw_response` and so no finish reason.
    ///
    /// Two turns here: the driver calls `run_kaish` (a provider reporting
    /// `finish_reason: "tool_calls"`, nested under `choices[]` the OpenAI way), then
    /// answers (an Anthropic-shaped top-level `stop_reason: "end_turn"`). Both land in
    /// the log, in order, each with the reason its own payload gave — two spellings
    /// through one extractor, from inside one real loop.
    #[tokio::test]
    async fn the_completion_log_carries_every_turn_including_inside_the_tool_loop() {
        const MODEL: &str = "driver";
        let client = ScriptedClient::builder()
            .on_model(MODEL, |req| {
                if transcript_text(req).contains("TOOL_RAN") {
                    // The answering turn: Anthropic's top-level spelling.
                    Ok(with_raw(
                        text_response("the answer"),
                        json!({"stop_reason": "end_turn"}),
                    ))
                } else {
                    // A turn INSIDE the loop: the OpenAI-compatible spelling, nested.
                    Ok(with_raw(
                        tool_call_response("c1", "run_kaish", json!({"script": "echo TOOL_RAN"})),
                        json!({"choices": [{"index": 0, "finish_reason": "tool_calls"}]}),
                    ))
                }
            })
            .build();

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let log = CompletionLog::new();
        let model = client.completion_model(MODEL);
        let (answer, _usage) = run_phase_logged(
            &log,
            &model,
            MODEL,
            "answer the question",
            16384,
            None,
            Message::user("q"),
            4,
            None,
            &crate::progress::NullSink,
            || {
                Ok(vec![traced(RunKaish::new(KaishWorker::spawn_with(
                    &root,
                    SandboxConfig::default(),
                )?))])
            },
            false,
        )
        .await
        .expect("the scripted loop answers");

        assert_eq!(answer, "the answer");
        let turns = log.turns();
        assert_eq!(
            turns.len(),
            2,
            "every completion is recorded, tool turn included: {turns:?}"
        );
        assert_eq!(
            turns[0].finish_reason.as_deref(),
            Some("tool_calls"),
            "the turn inside the loop — the one no hook can reach"
        );
        assert_eq!(turns[0].tool_calls, 1);
        assert_eq!(turns[0].text_chars, 0);
        assert_eq!(
            turns[1].finish_reason.as_deref(),
            Some("end_turn"),
            "a different provider spelling, same extractor"
        );
        assert_eq!(
            log.last_finish_reason().as_deref(),
            Some("end_turn"),
            "the phase's ending is one call away"
        );
    }

    /// oneshot carries the provider's reported token usage back out — the thin
    /// single-turn seam threads `PromptResponse.usage` the same as the loop does.
    #[tokio::test]
    async fn oneshot_returns_reported_usage() {
        const MODEL: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| {
                Ok(with_usage(text_response("done"), usage(321, 21)))
            })
            .build();
        let (answer, reported) = oneshot("q", &[], &arm(&client, MODEL), &PhaseContext::default())
            .await
            .unwrap();
        assert_eq!(answer, "done");
        assert_eq!(reported.input_tokens, 321);
        assert_eq!(reported.output_tokens, 21);
    }

    /// The load-bearing e2e: a scripted consult that delegates a sweep to `explore′`,
    /// reads a span itself, and answers — driving the *real* loop end to end with no
    /// network. This proves what the offline wiring test below cannot: the driver's
    /// `explore` tool call actually runs the nested explorer agent (which itself runs
    /// real kaish), and its report aggregates into `ConsultOutput.report`. If
    /// delegation silently broke, `report` would come back empty and this fails.
    #[tokio::test]
    async fn consult_delegates_to_explore_and_aggregates_the_report() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const REPORT: &str = "EXPLORER_REPORT: src/foo.rs:1 fn target_marker";

        let client = ScriptedClient::builder()
            // The consult driver: delegate first, then read a span itself, then answer.
            // Content-driven, so it's robust to the loop's turn structure: it decides
            // from what it has already been shown, not from a call counter.
            .on_model(SYNTH, |req| {
                assert!(has_tool(req, "run_kaish"), "driver must have run_kaish");
                assert!(has_tool(req, "explore"), "driver must have explore′");
                let seen = transcript_text(req);
                if !seen.contains("EXPLORER_REPORT") {
                    // Haven't delegated yet → delegate a broad sweep.
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "where is target_marker defined?" }),
                    ))
                } else if !seen.contains("target_marker() {}") {
                    // Report in hand, but confirm the span directly via run_kaish.
                    Ok(tool_call_response(
                        "t-read",
                        "run_kaish",
                        json!({ "script": "cat -n src/foo.rs" }),
                    ))
                } else {
                    // Have the report and the confirmed span → answer.
                    Ok(text_response(
                        "ANSWER: target_marker is defined at src/foo.rs:1.",
                    ))
                }
            })
            // The explorer sub-agent: run real kaish once, then write its report.
            .on_model(EXPLORER, |req| {
                // Only run_kaish — the explorer has no nested explore′.
                assert!(has_tool(req, "run_kaish"), "explorer must have run_kaish");
                assert!(!has_tool(req, "explore"), "explorer must NOT nest explore′");
                let seen = transcript_text(req);
                if !seen.contains("target_marker") {
                    Ok(tool_call_response(
                        "t-grep",
                        "run_kaish",
                        json!({ "script": "grep -rn target_marker src" }),
                    ))
                } else {
                    Ok(text_response(REPORT))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(
            "Where is target_marker defined?",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        // The driver concluded with its final answer.
        assert!(
            out.answer
                .contains("target_marker is defined at src/foo.rs:1"),
            "answer should be the driver's final text, got: {:?}",
            out.answer
        );
        // The teeth: the explorer's report aggregated into ConsultOutput.report. A
        // non-empty report here means the `explore` tool call genuinely drove the
        // nested explorer agent and the reports sink collected it.
        assert!(
            out.report.contains("EXPLORER_REPORT"),
            "explorer's report must aggregate into ConsultOutput.report, got: {:?}",
            out.report
        );

        // And the routing held: the cheap model saw the *report* preamble (explorer
        // role), the synth model saw the *consult* preamble (driver role).
        let explorer_reqs = client.requests_for(EXPLORER);
        assert!(
            !explorer_reqs.is_empty(),
            "explorer model was actually invoked"
        );
        assert!(
            explorer_reqs[0]
                .preamble
                .as_deref()
                .unwrap_or("")
                .contains("You are the explorer"),
            "explorer got the report preamble: {:?}",
            explorer_reqs[0].preamble
        );
        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs[0]
                .preamble
                .as_deref()
                .unwrap_or("")
                .contains("second tool, `explore`"),
            "driver got the consult preamble: {:?}",
            synth_reqs[0].preamble
        );
    }

    /// A consult's reported `usage` must be the synth loop's tokens **plus** every
    /// delegated `explore′` sweep's — the nested explorer runs inside the synth's tool
    /// loop, so its tokens never reach the synth's own `PromptResponse.usage` and a
    /// naive implementation would drop them. Each scripted completion "reports" a fixed
    /// per-call usage; rig sums `usage += resp.usage` across a run, so the expected
    /// total is (synth completions × synth-usage) + (explorer completions × explorer-usage),
    /// computed from the request log so it can't drift with the loop's turn count. The
    /// teeth: forget to fold the explorer sink in and the total comes back short by the
    /// explorer's share, and this fails.
    #[tokio::test]
    async fn consult_usage_sums_synth_and_delegated_explorer_tokens() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        const REPORT: &str = "EXPLORER_REPORT: src/foo.rs:1";
        // Distinct per-call footprints so a missing addend is obvious in the total.
        let synth_each = usage(100, 10);
        let explorer_each = usage(50, 5);

        let client = ScriptedClient::builder()
            .on_model(SYNTH, move |req| {
                let resp = if transcript_text(req).contains("EXPLORER_REPORT") {
                    text_response("ANSWER: done.")
                } else {
                    tool_call_response("t-explore", "explore", json!({ "question": "sweep" }))
                };
                Ok(with_usage(resp, synth_each))
            })
            .on_model(EXPLORER, move |_req| {
                Ok(with_usage(text_response(REPORT), explorer_each))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(
            "Where is target_marker defined?",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        // Expected = per-call usage × how many completions each model actually made.
        let ns = client.requests_for(SYNTH).len() as u64;
        let ne = client.requests_for(EXPLORER).len() as u64;
        assert!(ns >= 2, "driver should delegate then answer (≥2 turns), got {ns}");
        assert!(ne >= 1, "explorer should have been delegated at least one sweep");

        assert_eq!(
            out.usage.input_tokens,
            ns * 100 + ne * 50,
            "input tokens must sum synth + explorer completions"
        );
        assert_eq!(
            out.usage.output_tokens,
            ns * 10 + ne * 5,
            "output tokens must sum synth + explorer completions"
        );
        // And the explorer's share is genuinely included — the total strictly exceeds
        // the synth's own tokens, so dropping the nested sink would fail here.
        assert!(
            out.usage.input_tokens > ns * 100,
            "the delegated explorer's tokens must be folded into the consult total"
        );
    }

    /// Attachments reach the delegated sweep: a consult carrying attachments must
    /// hand every `explore′` sub-agent the read-them-WHOLE directive in its
    /// preamble — the sweep is a fresh agent that never saw the driver prompt, so
    /// without this it's blind to the very files the caller flagged as central.
    /// Inlined and oversize text attachments alike are listed; the directive is
    /// command voice with the paging idiom.
    #[tokio::test]
    async fn consult_attachments_reach_the_delegated_sweep_preamble() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("EXPLORER_REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "survey the change" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: done."))
                }
            })
            .on_model(EXPLORER, |_req| {
                Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig {
            attachments: vec![
                super::super::prompts::ConsultAttachment::Text {
                    path: "changes.diff".into(),
                    body: "-a\n+b".into(),
                },
                super::super::prompts::ConsultAttachment::TextOversize {
                    path: "src/big.rs".into(),
                    size: 900_000,
                },
            ],
            ..ConsultConfig::default()
        };

        consult_with(
            "Does the change hold up?",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let explorer_reqs = client.requests_for(EXPLORER);
        assert!(!explorer_reqs.is_empty(), "the sweep actually ran");
        let preamble = explorer_reqs[0].preamble.as_deref().unwrap_or("");
        assert!(
            preamble.contains("Read each one WHOLE"),
            "the sweep preamble carries the command-voice directive: {preamble:?}"
        );
        assert!(
            preamble.contains("- changes.diff") && preamble.contains("- src/big.rs"),
            "inlined and oversize attachments are both listed for the sweep: {preamble:?}"
        );
        assert!(
            !preamble.contains("-a\n+b"),
            "the sweep gets paths to read, never inlined bytes: {preamble:?}"
        );
    }

    // --- The explorer `attach` tool, wired into the recomposed consult loop ------

    /// A delegated sweep's OWN toolset (not the driver's) is exactly
    /// `{run_kaish, attach}` — never a nested `explore` (a sweep doesn't delegate
    /// further). Pinned by asserting inside the EXPLORER's own responder, since the
    /// inner toolset only exists once the driver actually delegates.
    #[tokio::test]
    async fn the_delegated_sweep_toolset_is_run_kaish_and_attach() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "q" }),
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .on_model(EXPLORER, |req| {
                assert!(has_tool(req, "run_kaish"), "{:?}", req.tools);
                assert!(has_tool(req, "attach"), "{:?}", req.tools);
                assert!(
                    !has_tool(req, "explore"),
                    "a sweep does not delegate further: {:?}",
                    req.tools
                );
                Ok(text_response("REPORT"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with("q", dir.path(), &arm(&client, EXPLORER), &arm(&client, SYNTH), &cfg)
            .await
            .expect("scripted consult should succeed");
    }

    /// The load-bearing wiring test: a sweep that calls `attach` must land its
    /// routed file — full body, numbered — on the DRIVER's tool result, not just
    /// in the sweep's own transcript.
    #[tokio::test]
    async fn a_sweep_attachment_rides_the_tool_result_to_the_driver() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("<file path=\"src/foo.rs\">") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the marker file" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: done"))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: src/foo.rs") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["src/foo.rs"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: routed the config file"))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(
            "attach the marker file",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs.iter().any(|r| r.transcript.contains("<file path=\"src/foo.rs\">")
                && r.transcript.contains("fn target_marker")),
            "the driver's transcript must carry the attached file's numbered body: {:?}",
            synth_reqs.iter().map(|r| &r.transcript).collect::<Vec<_>>()
        );
    }

    /// The explorer's OWN context (its later requests, after the attach call) must
    /// never carry the routed bytes — the whole premise of the feature is that the
    /// bytes bypass the sweep's own context.
    #[tokio::test]
    async fn the_explorer_never_sees_the_attached_bytes() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("REPORT: routed") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the marker file" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: done"))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: src/foo.rs") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["src/foo.rs"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: routed the config file"))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(
            "attach the marker file",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        for r in client.requests_for(EXPLORER) {
            assert!(
                !r.transcript.contains("fn target_marker"),
                "the explorer's own transcript must never carry the routed bytes: {:?}",
                r.transcript
            );
        }
    }

    /// A sweep that never calls `attach` hands the driver exactly the bare report —
    /// no evidence block appended, byte-for-byte the pre-attach behavior.
    #[tokio::test]
    async fn a_sweep_with_no_attachment_hands_the_driver_the_bare_report() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("PLAIN REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("PLAIN REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with("q", dir.path(), &arm(&client, EXPLORER), &arm(&client, SYNTH), &cfg)
            .await
            .expect("scripted consult should succeed");

        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs
                .iter()
                .any(|r| r.transcript.contains("PLAIN REPORT: src/foo.rs:1")
                    && !r.transcript.contains("Files the explorer routed")),
            "no attach call -> no evidence block, just the bare report: {:?}",
            synth_reqs.iter().map(|r| &r.transcript).collect::<Vec<_>>()
        );
    }

    /// Past `max_attachments`, the extra file's demotion reaches the DRIVER as a
    /// loud, consumer-shaped line — the driver learns it can still `run_kaish` the
    /// file itself.
    #[tokio::test]
    async fn an_over_cap_sweep_attach_reaches_the_driver_as_a_loud_demotion() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("could not route") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach two files" }),
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("1 of 1 attachments") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["src/foo.rs", "src/bar.rs"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: tried to route both"))
                }
            })
            .build();

        let dir = project_with_marker();
        fs::write(dir.path().join("src/bar.rs"), "fn other() {}\n").unwrap();
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                max_attachments: 1,
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };

        consult_with(
            "attach two files",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs
                .iter()
                .any(|r| r.transcript.contains("could not route")
                    && r.transcript.contains("budget of 1 was full")),
            "the over-cap demotion must reach the driver: {:?}",
            synth_reqs.iter().map(|r| &r.transcript).collect::<Vec<_>>()
        );
    }

    /// `explore` (v1) never offers `attach` — the top-level tool passes `None`
    /// straight through to `run_explore_phase`, so the toolset stays exactly
    /// `{run_kaish}`.
    #[tokio::test]
    async fn the_top_level_explore_sweep_offers_no_attach_tool() {
        const EXPLORER: &str = "cheap-explorer";
        let client = ScriptedClient::builder()
            .on_model(EXPLORER, |req| {
                assert!(
                    !has_tool(req, "attach"),
                    "the top-level explore tool must not offer attach"
                );
                Ok(text_response("REPORT"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ExploreConfig::default();
        explore_with(
            "q",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[],
            None,
        )
        .await
        .expect("scripted explore should succeed");
    }

    /// `max_attachments: 0` omits the `attach` tool from a delegated sweep's
    /// toolset entirely — the config-driven off switch, distinct from the v1
    /// top-level-`explore` exclusion above (this one is `consult`'s nested sweep).
    #[tokio::test]
    async fn max_attachments_zero_omits_the_attach_tool() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "q" }),
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .on_model(EXPLORER, |req| {
                assert!(
                    !has_tool(req, "attach"),
                    "max_attachments = 0 must omit the attach tool"
                );
                Ok(text_response("REPORT"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                max_attachments: 0,
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };

        consult_with("q", dir.path(), &arm(&client, EXPLORER), &arm(&client, SYNTH), &cfg)
            .await
            .expect("scripted consult should succeed");
    }

    /// A sweep's `attach` call surfaces as `PhaseEvent::Attached` on the shared
    /// progress sink — the one beat that lets an operator observe the pattern the
    /// generous default budget exists to watch for.
    #[tokio::test]
    async fn sweep_attachments_surface_as_progress() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("REPORT: routed") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the marker file" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: done"))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: src/foo.rs") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["src/foo.rs"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: routed the config file"))
                }
            })
            .build();

        let dir = project_with_marker();
        let sink = Arc::new(RecordingSink::default());
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    progress: sink.clone(),
                    ..PhaseContext::default()
                },
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };

        consult_with(
            "attach the marker file",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let events = sink.events();
        assert!(
            events.contains(&PhaseEvent::Attached {
                path: "src/foo.rs".into()
            }),
            "an attach call must surface as progress: {events:?}"
        );
    }

    /// A minimal PNG signature plus filler — enough to sniff as an image without
    /// decoding it (mirrors the `view_image`/`attach.rs`/`sweep_attach.rs` test helpers).
    fn fake_png_bytes() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(std::iter::repeat_n(0xAB, 16));
        v
    }

    /// A vision-capable driver (Anthropic/Gemini-shaped: `tool_result_images = true`)
    /// receives a sweep's routed image right on the `explore` tool result — no break,
    /// no user-turn rewrite, since this transport carries an image in a tool result.
    #[tokio::test]
    async fn a_vision_driver_receives_the_sweep_image_in_the_tool_result() {
        const SYNTH: &str = "vision-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let history: Vec<Message> = req.chat_history.iter().cloned().collect();
                if any_tool_result_image(&history) {
                    Ok(text_response("ANSWER: saw the diagram"))
                } else {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the diagram" }),
                    ))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: docs/arch.png") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["docs/arch.png"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: the diagram shows the pipeline"))
                }
            })
            .build();

        let dir = project_with_marker();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/arch.png"), fake_png_bytes()).unwrap();

        let cfg = ConsultConfig::default();
        // vision + tool_result_images: the Anthropic/Gemini shape.
        let synth = vision_arm(&client, SYNTH);

        consult_with(
            "attach the diagram",
            dir.path(),
            &arm(&client, EXPLORER),
            &synth,
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        assert!(
            client
                .requests_for(SYNTH)
                .iter()
                .any(|r| any_tool_result_image(&r.chat_history)),
            "the vision driver must receive the image on the explore tool result"
        );
    }

    /// A vision-capable driver whose TRANSPORT can't carry an image in a tool result
    /// (an OpenAI VLM shape: `tool_result_images = false`) still receives the sweep's
    /// image — on a separate user `Image` turn, via the same break-rewrite-resume
    /// path `view_image` uses, now generalized to any image-bearing tool result.
    #[tokio::test]
    async fn an_openai_vlm_driver_receives_the_sweep_image_on_a_user_turn() {
        const SYNTH: &str = "openai-vlm-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let history: Vec<Message> = req.chat_history.iter().cloned().collect();
                if user_image_messages(&history) > 0 {
                    Ok(text_response("ANSWER: saw the diagram"))
                } else {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the diagram" }),
                    ))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: docs/arch.png") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["docs/arch.png"] }),
                    ))
                } else {
                    Ok(text_response("REPORT: the diagram shows the pipeline"))
                }
            })
            .build();

        let dir = project_with_marker();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/arch.png"), fake_png_bytes()).unwrap();

        let cfg = ConsultConfig::default();
        // vision but NOT tool_result_images: the OpenAI VLM shape that must break,
        // rewrite, and resume.
        let synth = Arm::new(
            client.clone(),
            SYNTH,
            16384,
            None,
            ModelCaps {
                vision: true,
                tool_result_images: false,
            },
        );

        consult_with(
            "attach the diagram",
            dir.path(),
            &arm(&client, EXPLORER),
            &synth,
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let synth_reqs = client.requests_for(SYNTH);
        assert!(
            synth_reqs
                .iter()
                .any(|r| user_image_messages(&r.chat_history) > 0),
            "the OpenAI-shaped driver must receive the image on its own user turn: {:?}",
            synth_reqs.iter().map(|r| r.chat_history.len()).collect::<Vec<_>>()
        );
        assert!(
            synth_reqs
                .iter()
                .all(|r| !any_tool_result_image(&r.chat_history)),
            "an image must never ride this transport's tool-result channel"
        );
    }

    /// A blind consumer (the default `arm()` — not vision-capable) never receives an
    /// image: `attach` refuses it in the receipt, and the EXPLORER sees that refusal
    /// on its very next turn (immediately, within the same sweep) rather than only
    /// finding out once the whole sweep concludes.
    #[tokio::test]
    async fn a_blind_driver_refuses_the_image_and_the_explorer_learns_immediately() {
        const SYNTH: &str = "blind-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "attach the diagram" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: no diagram available"))
                }
            })
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("not attached: docs/arch.png") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["docs/arch.png"] }),
                    ))
                } else {
                    // The explorer learns of the refusal on its OWN very next turn —
                    // it doesn't have to wait for anything downstream.
                    assert!(
                        transcript_text(req).contains("reads text only"),
                        "the explorer must see the refusal reason immediately: {}",
                        transcript_text(req)
                    );
                    Ok(text_response("REPORT: could not attach the diagram"))
                }
            })
            .build();

        let dir = project_with_marker();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/arch.png"), fake_png_bytes()).unwrap();

        let cfg = ConsultConfig::default();

        consult_with(
            "attach the diagram",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH), // the default arm is blind
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        for r in client.requests_for(SYNTH) {
            assert!(
                !any_tool_result_image(&r.chat_history),
                "a blind driver must never receive an image on any channel"
            );
        }
    }

    /// The `explore` tool's phase, surfaced directly: `explore_with` runs ONE
    /// explorer arm over `{run_kaish}` against the real repo and returns the
    /// explorer's cited report *verbatim* — no synth, no second phase. Mirrors the
    /// consult e2e above but for the exposed evidence-gathering half. If the phase
    /// ever synthesized an answer instead of surfacing the report, the marker
    /// wouldn't survive; and the explorer must run on the report (explorer-role)
    /// preamble, not a driver preamble.
    #[tokio::test]
    async fn explore_with_runs_the_single_explorer_phase_and_returns_the_report() {
        const EXPLORER: &str = "cheap-explorer";
        const REPORT: &str = "EXPLORER_REPORT: src/foo.rs:1 fn target_marker";

        let client = ScriptedClient::builder()
            // A single-phase sweep: grep once against real kaish, then write the report.
            .on_model(EXPLORER, |req| {
                // Explorer-only — `run_kaish`, and no nested `explore′` (explore is one phase).
                assert!(has_tool(req, "run_kaish"), "explorer must have run_kaish");
                assert!(
                    !has_tool(req, "explore"),
                    "explore is single-phase — no nested explore′"
                );
                let seen = transcript_text(req);
                if !seen.contains("target_marker() {}") {
                    Ok(tool_call_response(
                        "t-grep",
                        "run_kaish",
                        json!({ "script": "grep -rn target_marker src" }),
                    ))
                } else {
                    Ok(text_response(REPORT))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ExploreConfig::default();

        let (report, _usage) = explore_with(
            "Where is target_marker defined?",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[],
            None,
        )
        .await
        .expect("scripted explore should succeed");

        // The teeth: the result IS the explorer's report, surfaced verbatim — not a
        // synthesized answer. A synth phase would have replaced the marker.
        assert!(
            report.contains("EXPLORER_REPORT"),
            "explore must return the explorer's report itself, got: {report:?}"
        );

        // Routing: the one arm ran on the report (explorer-role) preamble.
        let reqs = client.requests_for(EXPLORER);
        assert!(!reqs.is_empty(), "explorer model was actually invoked");
        assert!(
            reqs[0]
                .preamble
                .as_deref()
                .unwrap_or("")
                .contains("You are the explorer"),
            "explorer got the report preamble: {:?}",
            reqs[0].preamble
        );
    }

    /// The top-level `explore` tool's attachments land the same way: a read-WHOLE
    /// directive appended to the explorer preamble — never inlined bytes.
    #[tokio::test]
    async fn explore_with_appends_the_attachment_directive() {
        const EXPLORER: &str = "cheap-explorer";
        let client = ScriptedClient::builder()
            .on_model(EXPLORER, |_req| {
                Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ExploreConfig::default();
        explore_with(
            "survey the parser",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[super::super::prompts::ConsultAttachment::TextOversize {
                path: "src/parser_gen.rs".into(),
                size: 900_000,
            }],
            None,
        )
        .await
        .expect("scripted explore should succeed");

        let preamble = client.requests_for(EXPLORER)[0]
            .preamble
            .clone()
            .unwrap_or_default();
        assert!(
            preamble.contains("Read each one WHOLE") && preamble.contains("- src/parser_gen.rs"),
            "explore's preamble carries the directive: {preamble:?}"
        );
    }

    /// The `explore` phase is a *single* sweep, not a nested delegation — so its
    /// progress shape is the explorer's own reads reaching the sink directly, with
    /// **no** `SweepStarted`/`SweepFinished` bracket (that bracket is `RunExplore`'s,
    /// emitted only when the consult driver delegates a sub-agent). This is the teeth
    /// for the seam split: if the bracket ever migrated from `RunExplore::call` into
    /// the shared `run_explore_phase`, a `SweepStarted` would appear here and this
    /// test fails; and if the sink weren't threaded through, the explorer's `run_kaish`
    /// read would be missing.
    #[tokio::test]
    async fn explore_progress_surfaces_the_read_with_no_sweep_bracket() {
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("exit:") {
                    Ok(tool_call_response(
                        "t-grep",
                        "run_kaish",
                        json!({ "script": "grep -rn target_marker src" }),
                    ))
                } else {
                    Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let sink = Arc::new(RecordingSink::default());
        let cfg = ExploreConfig {
            phase: PhaseContext {
                progress: sink.clone(),
                ..PhaseContext::default()
            },
            ..ExploreConfig::default()
        };

        explore_with(
            "Where is target_marker defined?",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[],
            None,
        )
        .await
        .expect("scripted explore should succeed");

        let events = sink.events();
        // The explorer's own read reached the sink (single-phase threading works).
        assert!(
            events.contains(&PhaseEvent::KaishRun {
                script: "grep -rn target_marker src".into()
            }),
            "the explorer's read must surface through the single phase: {events:?}"
        );
        // No sweep bracket: explore is not a sub-agent delegation. A migrated bracket
        // would light these up.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PhaseEvent::SweepStarted { .. })),
            "explore is single-phase — no SweepStarted bracket belongs here: {events:?}"
        );
        assert!(
            !events.contains(&PhaseEvent::SweepFinished),
            "explore is single-phase — no SweepFinished bracket belongs here: {events:?}"
        );
    }

    /// The `direct` lane: `deliberate_direct` runs the offline synth as ONE toolless
    /// turn over the dossier and returns its deliberation. Teeth: the synth must be
    /// toolless (no `run_kaish`/`explore` — it reasons over the handed dossier, it
    /// does not investigate), the dossier must reach it, and the result is the synth's
    /// answer. This is the local-lane execution the `direct` cast finally routes to.
    #[tokio::test]
    async fn deliberate_direct_runs_one_toolless_turn_over_the_dossier() {
        const SYNTH: &str = "big-local-synth";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                assert!(
                    !has_tool(req, "run_kaish") && !has_tool(req, "explore"),
                    "the direct synth deliberates toolless over the dossier"
                );
                let seen = transcript_text(req);
                assert!(
                    seen.contains("DOSSIER_MARKER"),
                    "the built dossier must reach the synth: {seen}"
                );
                Ok(text_response(
                    "DELIBERATION: the retry path is safe because …",
                ))
            })
            .build();

        let cfg = PhaseContext::default();
        let (out, _usage) = deliberate_direct(
            "Is the retry path safe?",
            "src/retry.rs:12 DOSSIER_MARKER fn retry()",
            &[],
            &arm(&client, SYNTH),
            "You are the synthesis agent, answering a hard question offline.",
            cfg.call_deadline,
        )
        .await
        .expect("scripted direct deliberation should succeed");

        assert!(
            out.contains("DELIBERATION"),
            "returns the synth's deliberation verbatim: {out}"
        );
        // Exactly one upstream request — the single offline turn, no follow-up.
        assert_eq!(
            client.requests_for(SYNTH).len(),
            1,
            "one toolless completion"
        );
    }

    /// `deliberate`'s dossier stage: `explore_with` run with an attach sink returns
    /// the RAW dossier text (unstitched — `explore_with` itself doesn't touch the
    /// sink); the caller drains it and stitches `sweep_evidence_block` on top, the
    /// exact composition `server::deliberate` performs. Proves that composition
    /// works end to end at the engine layer, without the MCP server.
    #[tokio::test]
    async fn a_deliberate_dossier_sweep_routes_bytes_into_the_dossier() {
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(EXPLORER, |req| {
                if !transcript_text(req).contains("attached: src/foo.rs") {
                    Ok(tool_call_response(
                        "t-attach",
                        "attach",
                        json!({ "paths": ["src/foo.rs"] }),
                    ))
                } else {
                    Ok(text_response("DOSSIER REPORT: src/foo.rs is the relevant module"))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ExploreConfig::default();
        let consumer = SweepConsumer {
            kind: SweepConsumerKind::OfflineSynth,
            label: Arc::from("the offline synth (`test-model`)"),
            vision: false,
        };
        let sink = Arc::new(SweepAttachSink::new(
            cfg.max_attachments,
            consumer.clone(),
            HashSet::new(),
        ));

        let (mut dossier, _usage) = explore_with(
            "what's the relevant module?",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[],
            Some(&sink),
        )
        .await
        .expect("scripted explore should succeed");

        // explore_with itself doesn't stitch — the raw dossier is just the report.
        assert!(
            !dossier.contains("<file path=\"src/foo.rs\">"),
            "explore_with returns the RAW dossier; stitching is the caller's job: {dossier:?}"
        );

        let delivery = sink.drain();
        if let Some(block) = sweep_evidence_block(&consumer, &delivery) {
            dossier.push_str(&block);
        }

        assert!(
            dossier.contains("DOSSIER REPORT")
                && dossier.contains("<file path=\"src/foo.rs\">")
                && dossier.contains("fn target_marker"),
            "the stitched dossier must carry both the report and the routed file's \
             numbered body: {dossier:?}"
        );
    }

    /// `deliberate_direct`'s single turn carries a routed image as a native part —
    /// the same `user_turn_with_attachments` seam `oneshot` uses, so a dossier
    /// sweep's `attach`-routed image reaches the direct-lane synth even though the
    /// synth itself never runs a tool loop.
    #[tokio::test]
    async fn deliberate_direct_carries_sweep_images_into_its_single_turn() {
        const SYNTH: &str = "big-local-vision-synth";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let history: Vec<Message> = req.chat_history.iter().cloned().collect();
                assert!(
                    user_image_messages(&history) > 0,
                    "the single turn must carry the routed image"
                );
                Ok(text_response("DELIBERATION: the diagram confirms it"))
            })
            .build();

        let image = Attachment::Image {
            path: "docs/arch.png".into(),
            mime: "image/png",
            data_b64: "ZmFrZQ==".into(),
        };
        let cfg = PhaseContext::default();
        let (out, _usage) = deliberate_direct(
            "Does the diagram confirm the design?",
            "src/x.rs:1 DOSSIER",
            &[image],
            &vision_arm(&client, SYNTH),
            "You are a capable model answering a hard question offline.",
            cfg.call_deadline,
        )
        .await
        .expect("scripted direct deliberation should succeed");

        assert!(out.contains("DELIBERATION"));
    }

    /// The wall-clock backstop: a wedged provider (a stopped/hung backend whose
    /// completion never returns — the 2026-07-02 failure mode) must abort a `consult`
    /// by `call_deadline`, not hang the caller forever. The synth model hangs; a tiny
    /// deadline should turn that into a prompt error. The outer guard is the teeth: it
    /// fails the test *fast* if the deadline isn't enforced (an unbounded `consult_with`
    /// would otherwise hang this test until CI's own timeout).
    #[tokio::test]
    async fn consult_aborts_when_a_backend_wedges() {
        const SYNTH: &str = "wedged-synth";
        const EXPLORER: &str = "cheap-explorer";
        let dir = tempdir().unwrap();

        // The synth's very first completion never returns; the explorer is never reached.
        let client = ScriptedClient::builder().hang_model(SYNTH).build();
        let mut cfg = ConsultConfig::default();
        cfg.explore.phase.call_deadline = Duration::from_millis(50);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            consult_with(
                "q",
                dir.path(),
                &arm(&client, EXPLORER),
                &arm(&client, SYNTH),
                &cfg,
            ),
        )
        .await
        .expect("consult_with did not honor call_deadline — it hung past 5s (no backstop)");

        let err = outcome.expect_err("a wedged backend must abort the consult, not answer");
        // Render the whole chain, exactly as the server's `consultation_failure_text`
        // does (`{err:#}`) — so this asserts what the *client* actually sees.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("deadline"),
            "the abort must name the wall-clock deadline, got: {msg}"
        );

        // The request did go out — the deadline aborted a real in-flight call, and the
        // wedge is exactly one completion that never returned (not a loop, not a no-op).
        assert_eq!(
            client.requests_for(SYNTH).len(),
            1,
            "the synth completion should have been dispatched once and then hung"
        );
    }

    /// The same wall-clock backstop guards `explore`, not just `consult`: a wedged
    /// explorer must abort by `call_deadline`. Guards against a refactor dropping the
    /// wrap from `explore_with` specifically (it shares `with_call_deadline`, but the
    /// call site is its own). Same outer-guard teeth as the consult version.
    #[tokio::test]
    async fn explore_aborts_when_the_explorer_wedges() {
        const EXPLORER: &str = "wedged-explorer";
        let dir = tempdir().unwrap();
        let client = ScriptedClient::builder().hang_model(EXPLORER).build();
        let mut cfg = ExploreConfig::default();
        cfg.phase.call_deadline = Duration::from_millis(50);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            explore_with(
                "q",
                dir.path().to_path_buf(),
                &arm(&client, EXPLORER),
                &cfg,
                &[],
                None,
            ),
        )
        .await
        .expect("explore_with did not honor call_deadline — it hung past 5s (no backstop)");

        let err = outcome.expect_err("a wedged explorer must abort the explore, not answer");
        assert!(
            format!("{err:#}").contains("deadline"),
            "the abort must name the wall-clock deadline, got: {err:#}"
        );
    }

    /// `deliberate`'s direct lane is async but NOT unbounded: it's an in-process
    /// completion kaibo holds, so a wedged local synth must abort by its deadline
    /// rather than leave the `job-N` running forever (so `job_wait`/`job_get` resolve
    /// within it). Only the batch lane — where kaibo holds no wait — escapes.
    #[tokio::test]
    async fn deliberate_direct_aborts_when_the_local_synth_wedges() {
        const SYNTH: &str = "wedged-local-synth";
        let client = ScriptedClient::builder().hang_model(SYNTH).build();

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            deliberate_direct(
                "q",
                "src/x.rs:1 DOSSIER",
                &[],
                &arm(&client, SYNTH),
                "offline synth preamble",
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("deliberate_direct did not honor its deadline — it hung past 5s (no backstop)");

        let err = outcome.expect_err("a wedged local synth must abort the deliberation, not answer");
        assert!(
            format!("{err:#}").contains("deadline"),
            "the abort must name the wall-clock deadline, got: {err:#}"
        );
    }

    /// Progress reaches the *deep* loop. The same delegate-then-read flow as the e2e
    /// above, but driven through a [`RecordingSink`] on `ConsultConfig`: the sink must
    /// see the sweep bracket (start/finish), the nested explorer's own `run_kaish`
    /// read, and the driver's direct `run_kaish` read. This is the teeth for the whole
    /// threading job — were the sink dropped anywhere between `ConsultConfig` and the
    /// tools (or not forwarded into the nested explorer), one of these would be missing.
    #[tokio::test]
    async fn progress_events_reach_the_sweep_and_both_kaish_reads() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                let seen = transcript_text(req);
                if !seen.contains("EXPLORER_REPORT") {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "where is target_marker defined?" }),
                    ))
                } else if !seen.contains("target_marker() {}") {
                    Ok(tool_call_response(
                        "t-read",
                        "run_kaish",
                        json!({ "script": "cat -n src/foo.rs" }),
                    ))
                } else {
                    Ok(text_response("ANSWER: src/foo.rs:1"))
                }
            })
            .on_model(EXPLORER, |req| {
                // Branch on tool *output* (run_kaish prefixes "exit:"), not on the
                // question text — the sub-question itself contains "target_marker", so
                // a content check on that would skip the read we're here to observe.
                if !transcript_text(req).contains("exit:") {
                    Ok(tool_call_response(
                        "t-grep",
                        "run_kaish",
                        json!({ "script": "grep -rn target_marker src" }),
                    ))
                } else {
                    Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let sink = Arc::new(RecordingSink::default());
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    progress: sink.clone(),
                    ..PhaseContext::default()
                },
                ..ExploreConfig::default()
            },
            ..ConsultConfig::default()
        };

        consult_with(
            "Where is target_marker?",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let events = sink.events();
        assert!(
            events.contains(&PhaseEvent::SweepStarted {
                question: "where is target_marker defined?".into()
            }),
            "the delegation must announce its start: {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::SweepFinished),
            "the delegation must announce its finish: {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::KaishRun { script: "grep -rn target_marker src".into() }),
            "the nested explorer's read must surface (sink threaded into the sub-agent): {events:?}"
        );
        assert!(
            events.contains(&PhaseEvent::KaishRun {
                script: "cat -n src/foo.rs".into()
            }),
            "the driver's own direct read must surface: {events:?}"
        );
        // Ordering sanity: the sweep starts before its nested read, which precedes the
        // sweep finishing — the bracket actually brackets.
        let pos = |want: &PhaseEvent| events.iter().position(|e| e == want).unwrap();
        let start = pos(&PhaseEvent::SweepStarted {
            question: "where is target_marker defined?".into(),
        });
        let nested = pos(&PhaseEvent::KaishRun {
            script: "grep -rn target_marker src".into(),
        });
        let finish = pos(&PhaseEvent::SweepFinished);
        assert!(
            start < nested && nested < finish,
            "sweep must bracket its nested read: {events:?}"
        );
    }

    /// A stateless consult (default `ConsultConfig`) emits to the `NullSink` — no
    /// panic, no observable effect. The opt-out path stays a true no-op.
    #[tokio::test]
    async fn the_default_sink_is_a_silent_no_op() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        // No token, no recording sink — just prove the default path runs clean.
        consult_with(
            "q",
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();
    }

    /// Multi-sweep: a driver that delegates to `explore′` more than once must
    /// aggregate every report into `ConsultOutput.report`, joined by the `---`
    /// separator. The single-delegation e2e can't see this — one report makes any
    /// join string look right.
    #[tokio::test]
    async fn multiple_sweeps_aggregate_into_one_report_joined_by_separator() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                // Delegate twice (distinguishable sub-questions), then answer. Count
                // the reports already gathered to decide which step we're on.
                let sweeps = transcript_text(req).matches("REPORT-").count();
                match sweeps {
                    0 => Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find the sandbox" }),
                    )),
                    1 => Ok(tool_call_response(
                        "s2",
                        "explore",
                        json!({ "question": "find the kaish syntax" }),
                    )),
                    _ => Ok(text_response("ANSWER from both sweeps")),
                }
            })
            .on_model(EXPLORER, |req| {
                // Each sweep answers immediately with a distinguishable report, keyed
                // off its sub-question (which is the explorer's whole prompt).
                if transcript_text(req).contains("sandbox") {
                    Ok(text_response("REPORT-SANDBOX: src/sandbox.rs:1"))
                } else {
                    Ok(text_response("REPORT-KAISH: src/kaish_syntax.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(
            "two-part question",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        assert!(
            out.report.contains("REPORT-SANDBOX"),
            "first sweep present: {:?}",
            out.report
        );
        assert!(
            out.report.contains("REPORT-KAISH"),
            "second sweep present: {:?}",
            out.report
        );
        assert_eq!(
            out.report.matches("---").count(),
            1,
            "exactly one `---` between two reports, got: {:?}",
            out.report
        );
        assert_eq!(
            client.requests_for(EXPLORER).len(),
            2,
            "the driver must have delegated two distinct sweeps"
        );
    }

    /// A dying `explore′` sweep must not sink the whole consult: the driver sees the
    /// failure in its transcript and answers from what it has, the report sink stays
    /// empty, and — the teeth for the harness's record-before-respond promise — the
    /// failed explorer request is still logged.
    #[tokio::test]
    async fn a_failed_sweep_surfaces_and_the_driver_recovers() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                // Delegate once; once the failure has come back, answer from direct work.
                if transcript_text(req).contains("simulated provider outage") {
                    Ok(text_response(
                        "ANSWER: explore failed, answered from direct reads",
                    ))
                } else {
                    Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                }
            })
            .on_model(EXPLORER, |_req| {
                Err(provider_error("simulated provider outage"))
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        let out = consult_with(
            "a question",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("a failed sweep must not fail the whole consult");

        // The driver only reaches this answer by *seeing* the error in its transcript
        // (its branch requires it) — so the answer text proves the failure surfaced.
        assert!(
            out.answer.contains("answered from direct reads"),
            "driver should recover after the sweep failed, got: {:?}",
            out.answer
        );
        // The sweep errored before `RunExplore` pushed to the sink, so the report is
        // empty — distinct from a sweep that ran and found nothing.
        assert!(
            out.report.is_empty(),
            "a failed sweep contributes no report: {:?}",
            out.report
        );
        // Record-before-respond: the failing explorer call was still captured.
        assert!(
            !client.requests_for(EXPLORER).is_empty(),
            "the failed explorer request must still be logged"
        );
    }

    /// Turn-cap recovery, offline: a model that never stops calling tools must still
    /// yield an answer. We cap the loop low and script a driver that *always* calls a
    /// tool — until it's shown `ToolChoice::None` (the forced finalize turn), where it
    /// writes its answer. This proves `run_phase` → `finalize_after_max_turns` turns a
    /// `MaxTurnsError` into a real answer from the partial transcript, a path the live
    /// tests can only hit by luck.
    #[tokio::test]
    async fn turn_cap_forces_a_final_answer_from_partial_work() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    // Forbidden from calling tools — answer from what we have.
                    Ok(text_response("FORCED FINAL ANSWER: src/foo.rs:1"))
                } else {
                    // Keep burning turns; never conclude on our own.
                    Ok(tool_call_response(
                        "t",
                        "run_kaish",
                        json!({ "script": "cat src/foo.rs" }),
                    ))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig {
            synth_max_turns: 2,
            ..ConsultConfig::default()
        };

        let out = consult_with(
            "A question the model never finishes answering",
            dir.path(),
            &arm(&client, "explorer-unused"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("turn-cap recovery should still produce an answer");

        assert!(
            out.answer.contains("FORCED FINAL ANSWER"),
            "the forced finalize turn must produce the answer, got: {:?}",
            out.answer
        );
        // And the recovery path actually ran: some request carried ToolChoice::None.
        let finalize = client
            .requests_for(SYNTH)
            .into_iter()
            .find(|r| r.tool_choice == Some(ToolChoice::None))
            .expect("a forced finalize turn (ToolChoice::None) must have been issued");
        // The teeth: the finalize turn must carry the *partial work* — the run_kaish
        // results the driver accumulated before the cap — not a blank history. The
        // answer is hardcoded in the responder, so without this a regression that fed
        // `finalize_after_max_turns` an empty transcript would still pass.
        assert!(
            finalize.transcript.contains("target_marker"),
            "finalize turn must replay the accumulated tool work, got transcript: {:?}",
            finalize.transcript
        );
    }

    // --- the silent-empty-answer battery (GH: consult returns just the footer) -------
    //
    // Reported 2026-08-01: a `consult_submit` on cast `deepseek` burned 16,801 output
    // tokens and returned an EMPTY answer body — just the provenance footer — while the
    // same question on `or-kimi` returned a full cited review. The generation happened;
    // nothing reached the caller. These four tests pin the shapes that produce an empty
    // final answer with **no error**, which is the worst possible failure: the calling
    // agent merges on a review that silently did not happen.
    //
    // All four are RED on purpose until the guard lands. The fix is *not* to make the
    // loop invent text — it is to fail the call loudly with the diagnostics attached,
    // exactly as the batch lane already does (`batch.rs::finish_gated_answer`, GH #75).

    /// **The DeepSeek repro.** A reasoning-only terminal turn: `reasoning_content` with
    /// an empty `content`. rig's deepseek converter drops the empty text block and emits
    /// a choice holding only `AssistantContent::Reasoning`
    /// (`rig-core/src/providers/deepseek.rs:400,:420`); rig sees no tool calls, so it
    /// takes the clean-finish branch and extracts text with `assistant_text_from_choice`,
    /// which filters to `Text` and yields `""`
    /// (`rig-core/src/agent/prompt_request/mod.rs:569`, `:887`, `:918`). `run_phase`
    /// passes that straight through (`engine.rs:862`), and every layer above — the
    /// provenance footer, the job result — happily renders an empty answer.
    ///
    /// A consult that produced no answer must never be a successful empty. With evidence
    /// already gathered, the recovery is to ask once for the write-up it owed — which is
    /// what job-1 needed: the driver held an explorer report and fifteen greps, and
    /// stopped mid-investigation with a 14-token terminal turn.
    ///
    /// **Gate direction 1 of 2: evidence present ⇒ retry, and the retry's answer stands.**
    #[tokio::test]
    async fn an_empty_answer_with_evidence_is_recovered_by_one_forced_write_up_turn() {
        const SYNTH: &str = "deepseek-v4-pro";
        const RECOVERED: &str = "REVIEW: src/foo.rs:1 defines the marker.";
        let client = ScriptedClient::builder()
            // Sweep once (so real evidence and real tokens are on the record), "answer"
            // with reasoning only — the exact wire shape that produced the empty body —
            // then write the review when forced to conclude.
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    Ok(with_usage(text_response(RECOVERED), usage(36_104, 3_971)))
                } else if transcript_text(req).contains("SWEEP_DONE") {
                    Ok(with_usage(
                        reasoning_response("I have finished reviewing. Now to write it up..."),
                        usage(1_164_958, 16_801),
                    ))
                } else {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "review the diff" }),
                    ))
                }
            })
            .on_model("cheap-explorer", |_req| Ok(text_response("SWEEP_DONE")))
            .build();

        let dir = project_with_marker();
        let out = consult_with(
            "review this branch",
            dir.path(),
            &arm(&client, "cheap-explorer"),
            &arm(&client, SYNTH),
            &ConsultConfig::default(),
        )
        .await
        .expect("an empty answer backed by real evidence must be recovered, not failed");

        assert!(
            out.answer.contains(RECOVERED),
            "the forced write-up turn's answer must be what the caller gets: {:?}",
            out.answer
        );

        // Exactly ONE forced turn — the recovery is bounded, not a retry loop.
        let finalizes: Vec<_> = client
            .requests_for(SYNTH)
            .into_iter()
            .filter(is_finalize_request)
            .collect();
        assert_eq!(
            finalizes.len(),
            1,
            "the empty answer must buy exactly one forced write-up turn"
        );

        // It must carry the accumulated evidence — a forced turn over a blank history
        // would just invite the ungrounded answer the gate exists to refuse.
        assert!(
            finalizes[0].transcript.contains("SWEEP_DONE"),
            "the forced turn must replay the gathered evidence, got: {:?}",
            finalizes[0].transcript
        );
        // ...and it must say *stopped without answering*, not *out of turns*. The model
        // had 198 turns left; telling it otherwise is a lie that shapes its answer.
        assert!(
            finalizes[0].transcript.contains("came back empty"),
            "the forced turn must carry EMPTY_ANSWER_NOTE: {:?}",
            finalizes[0].transcript
        );
        assert!(
            !finalizes[0].transcript.contains("reached your research limit"),
            "the forced turn must NOT claim a turn cap that was never hit: {:?}",
            finalizes[0].transcript
        );

        // The retry's usage is ADDED to what was already spent, never substituted —
        // the caller paid for both, so the footer must say so. (16,801 + 3,971.)
        assert_eq!(
            out.usage.output_tokens, 20_772,
            "the pre-retry spend must survive into the returned usage"
        );
        assert_eq!(out.usage.input_tokens, 1_201_062);
    }

    /// **Gate direction 2 of 2: no evidence ⇒ error, and no forced turn is even attempted.**
    ///
    /// This is the load-bearing half. A forced "write it now" handed to a model that
    /// gathered *nothing* invites it to comply anyway, producing a confident ungrounded
    /// answer on a lane whose whole product is grounded citation — strictly worse than an
    /// error. So we assert the *absence* of the second request, not merely that the call
    /// failed: an implementation that retried blind and happened to get an empty answer
    /// back would pass an error-only assertion while being exactly wrong.
    #[tokio::test]
    async fn an_empty_answer_with_no_evidence_errors_without_asking_the_model_again() {
        const SYNTH: &str = "deepseek-v4-pro";
        let client = ScriptedClient::builder()
            // Straight to a reasoning-only turn: no tool calls, so no tool results, so
            // nothing to write up. The raw payload carries the provider's own
            // finish_reason, the way a real deepseek response would — the error must
            // surface it.
            .on_model(SYNTH, |_req| {
                Ok(with_raw(
                    with_usage(
                        reasoning_response("Hmm, where should I even start..."),
                        usage(6_342, 199),
                    ),
                    serde_json::json!({"choices": [{"finish_reason": "stop"}]}),
                ))
            })
            .build();

        let dir = project_with_marker();
        let err = consult_with(
            "review this branch",
            dir.path(),
            &arm(&client, "explorer-unused"),
            &arm(&client, SYNTH),
            &ConsultConfig::default(),
        )
        .await
        .expect_err("an empty answer with no evidence gathered must fail");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("no tool results"),
            "the error must name the reason the retry was withheld: {msg}"
        );
        // The teeth: the model was never asked to answer anyway.
        assert!(
            !client.requests_for(SYNTH).iter().any(is_finalize_request),
            "a model with no evidence must NOT be asked to write an answer anyway — \
             that invites a fabricated review, which is worse than an error"
        );
        // And the diagnostics still ride along.
        assert!(
            msg.contains("199"),
            "the error must carry the output-token count: {msg}"
        );
        assert!(
            msg.contains("finish_reason \"stop\""),
            "the error must name the provider's own finish_reason: {msg}"
        );
    }

    /// At most once, proven: evidence is present so the retry fires, the forced turn *also*
    /// comes back empty, and that is an error — not a second re-ask. `run_phase`'s
    /// empty-answer arm returns on every path, so the bound is structural rather than
    /// flag-enforced; this is what would fail if a later edit turned it into a loop.
    #[tokio::test]
    async fn an_empty_forced_write_up_turn_errors_and_is_never_retried_twice() {
        const SYNTH: &str = "deepseek-v4-pro";
        let client = ScriptedClient::builder()
            // Reasoning-only forever, including when forced to conclude.
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("SWEEP_DONE") {
                    Ok(with_usage(
                        reasoning_response("...still thinking..."),
                        usage(1_000, 500),
                    ))
                } else {
                    Ok(tool_call_response(
                        "t-explore",
                        "explore",
                        json!({ "question": "review the diff" }),
                    ))
                }
            })
            .on_model("cheap-explorer", |_req| Ok(text_response("SWEEP_DONE")))
            .build();

        let dir = project_with_marker();
        let err = consult_with(
            "review this branch",
            dir.path(),
            &arm(&client, "cheap-explorer"),
            &arm(&client, SYNTH),
            &ConsultConfig::default(),
        )
        .await
        .expect_err("a forced write-up turn that is also empty must fail, not re-ask again");

        assert!(
            format!("{err:#}").contains("came back empty again"),
            "the error must name the exhausted recovery: {err:#}"
        );
        assert_eq!(
            client
                .requests_for(SYNTH)
                .into_iter()
                .filter(is_finalize_request)
                .count(),
            1,
            "exactly one forced turn — the recovery must never stack"
        );
    }

    /// The same hole through the *other* door: a terminal turn whose text block is
    /// present but empty. rig normalizes that to "no assistant output"
    /// (`is_empty_assistant_turn`, `prompt_request/mod.rs:561`) and still returns
    /// `Ok` with `output == ""` — rig's own
    /// `prompt_request_stops_cleanly_on_empty_terminal_turn` test pins that as intended
    /// upstream behavior. So kaibo cannot delegate this check to rig; the guard has to
    /// live at `run_phase`'s `Ok` arm. A provider-independent shape: any model that
    /// finishes with no text lands here.
    #[tokio::test]
    async fn an_empty_text_terminal_turn_fails_loudly_instead_of_answering_empty() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |_req| {
                Ok(with_usage(text_response("   \n  "), usage(1000, 9000)))
            })
            .build();

        let dir = tempdir().unwrap();
        let err = consult_with(
            "q",
            dir.path(),
            &arm(&client, "explorer-unused"),
            &arm(&client, SYNTH),
            &ConsultConfig::default(),
        )
        .await
        .expect_err("a whitespace-only answer is no answer — it must fail, not succeed empty");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("empty") || msg.contains("no answer"),
            "the error must name the empty answer: {msg}"
        );
    }

    /// Amy's candidate 1, checked at the place it actually bites. `run_phase` *does*
    /// recover from a turn cap — it runs a forced finalize turn
    /// (`finalize_after_max_turns`, `engine.rs:901`) rather than returning the last
    /// message blindly. But that turn's answer is extracted the same way
    /// (`engine.rs:935`), so a finalize turn that produces only reasoning is *also* a
    /// silent empty — and the turn-cap hit itself never reaches the caller (it's a
    /// progress beat only, `engine.rs:812,:866`). Burning the whole budget and
    /// delivering nothing must be loud, and must name the cap.
    #[tokio::test]
    async fn a_reasoning_only_finalize_turn_after_the_turn_cap_fails_loudly() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    Ok(with_usage(
                        reasoning_response("still thinking about how to start..."),
                        usage(500_000, 12_000),
                    ))
                } else {
                    Ok(tool_call_response(
                        "t",
                        "run_kaish",
                        json!({ "script": "cat src/foo.rs" }),
                    ))
                }
            })
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig {
            synth_max_turns: 2,
            ..ConsultConfig::default()
        };
        let err = consult_with(
            "a question the model never finishes answering",
            dir.path(),
            &arm(&client, "explorer-unused"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect_err(
            "spending the whole turn budget and then delivering no text must be an \
             error, not an empty success",
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains('2'),
            "the error must name the turn cap that was hit: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("empty") || msg.to_lowercase().contains("no answer"),
            "the error must name the empty answer: {msg}"
        );
    }

    /// The fourth door, which the report shape hides: the **explorer** phase. The
    /// top-level `explore` tool returns its report verbatim, so a reasoning-only
    /// explorer turn ships a report body that is just the footer — same silent shape,
    /// different tool. (Inside `consult` this lands as an empty `explore′` tool result,
    /// which is arguably worse: the driver reasons on a blank sweep and never learns
    /// the sweep failed.)
    #[tokio::test]
    async fn a_reasoning_only_explorer_report_fails_loudly_instead_of_reporting_empty() {
        const EXPLORER: &str = "deepseek-v4-flash";
        let client = ScriptedClient::builder()
            .on_model(EXPLORER, |_req| {
                Ok(with_usage(
                    reasoning_response("I have surveyed the repo and concluded..."),
                    usage(200_000, 4_000),
                ))
            })
            .build();

        let dir = tempdir().unwrap();
        let err = explore_with(
            "where does the cast live?",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &ExploreConfig::default(),
            &[],
            None,
        )
        .await
        .expect_err("an explorer that reported nothing must fail, not return an empty report");
        // No tool results in its transcript, so no blind re-ask — the same gate.
        assert!(
            !client.requests_for(EXPLORER).iter().any(is_finalize_request),
            "an explorer with no evidence must not be asked to report anyway"
        );
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("empty") || msg.contains("no answer") || msg.contains("no report"),
            "the error must name the empty report: {msg}"
        );
    }

    /// The guard lives at the **shared seam** (`run_phase`), not on the `consult` path,
    /// so every tool built on that loop inherits it from one place. `oneshot` and
    /// `deliberate_direct` are the toolless lanes: they can never satisfy the evidence
    /// gate (no tools ⇒ no tool results), so for them an empty answer is *always* the
    /// error case — which is the right answer, since a toolless model asked to "write it
    /// now" has nothing but its own priors to write from.
    ///
    /// If a future refactor moves the guard up into `consult_with`, these two go silently
    /// empty again — that is exactly the regression this pins.
    #[tokio::test]
    async fn the_empty_answer_guard_covers_the_toolless_lanes_too() {
        const MODEL: &str = "reasoner";
        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| {
                Ok(with_usage(
                    reasoning_response("Considering the question..."),
                    usage(2_000, 800),
                ))
            })
            .build();

        let err = oneshot("q", &[], &arm(&client, MODEL), &PhaseContext::default())
            .await
            .expect_err("oneshot must not return an empty answer as a success");
        assert!(
            format!("{err:#}").contains("EMPTY answer"),
            "oneshot must fail with the shared empty-answer error: {err:#}"
        );

        let err = deliberate_direct(
            "q",
            "the dossier",
            &[],
            &arm(&client, MODEL),
            "system",
            Duration::from_secs(30),
        )
        .await
        .expect_err("deliberate_direct must not return an empty answer as a success");
        assert!(
            format!("{err:#}").contains("EMPTY answer"),
            "deliberate_direct must fail with the shared empty-answer error: {err:#}"
        );

        // Neither lane re-asked: no evidence is possible without tools, so the gate holds.
        assert!(
            !client.requests_for(MODEL).iter().any(is_finalize_request),
            "a toolless lane can never satisfy the evidence gate, so it must never re-ask"
        );
    }

    /// Thinking params must reach *every* model call — the consult driver and each
    /// nested `explore′`. All other tests pass `None` for thinking, so a regression
    /// that dropped `additional_params` in `run_phase`, or stopped `RunExplore`
    /// forwarding its arm's params to the nested loop, would slip through. These
    /// shapes are provider-specific and have already drifted once (`docs/issues.md`).
    #[tokio::test]
    async fn thinking_params_reach_both_the_driver_and_every_sweep() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        // Anthropic budget tier: both arms resolve to the same top-level `thinking`
        // block (the ids classify legacy/budget). The mock doesn't interpret it — we
        // only assert it survives the plumbing into *every* request, unchanged.
        let expected = json!({ "thinking": { "type": "enabled", "budget_tokens": 4096 } });

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("REPORT") {
                    Ok(text_response("ANSWER"))
                } else {
                    Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(
            "q",
            dir.path(),
            &arm_with(
                &client,
                EXPLORER,
                thinking_params(ProviderKind::Anthropic, EXPLORER, 4096),
            ),
            &arm_with(
                &client,
                SYNTH,
                thinking_params(ProviderKind::Anthropic, SYNTH, 4096),
            ),
            &cfg,
        )
        .await
        .unwrap();

        // Both roles were actually exercised (so the loop below isn't vacuous)...
        assert!(!client.requests_for(SYNTH).is_empty(), "driver ran");
        assert!(!client.requests_for(EXPLORER).is_empty(), "a sweep ran");
        // ...and every request carried the thinking shape, unchanged.
        for r in client.requests() {
            assert_eq!(
                r.additional_params.as_ref(),
                Some(&expected),
                "model {:?} must carry the thinking params, got: {:?}",
                r.model,
                r.additional_params
            );
        }
    }

    /// The `run_phase` span surfaces the exact reasoning params it ships, so a trace can
    /// show whether — and at what depth — thinking was on (the wire truth behind the
    /// `chat` spans' `reasoning_tokens`). Discriminating on both edges: a phase carrying
    /// `additional_params` records `gen_ai.request.thinking` equal to the blob it sent
    /// (here the on-theme Gemini `thinkingLevel`), and a toggle-less phase
    /// (`thinking == None`) records nothing — so a regression that always-records or
    /// never-records fails one branch. The field is filled in the body via
    /// `Span::current().record`, so the capture merges `on_record` into per-span state,
    /// exactly like the `tool` span harness.
    #[test]
    fn run_phase_span_carries_the_thinking_params() {
        use crate::test_support::serialized_capture;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        /// Grabs the one field we care about; a `%display` value lands as a debug value.
        #[derive(Default)]
        struct Grab(Option<String>);
        impl tracing::field::Visit for Grab {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "gen_ai.request.thinking" {
                    self.0 = Some(format!("{v:?}"));
                }
            }
        }
        #[derive(Default)]
        struct Stored(Option<String>);
        /// Collects, per closed `run_phase` span, whatever `gen_ai.request.thinking` held.
        #[derive(Clone, Default)]
        struct PhaseCapture(Arc<Mutex<Vec<Option<String>>>>);
        impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for PhaseCapture {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                id: &tracing::Id,
                ctx: Context<'_, S>,
            ) {
                if attrs.metadata().name() != "run_phase" {
                    return;
                }
                let mut g = Grab::default();
                attrs.record(&mut g); // Empty at open — the body records it later.
                ctx.span(id).unwrap().extensions_mut().insert(Stored(g.0));
            }
            fn on_record(
                &self,
                id: &tracing::Id,
                values: &tracing::span::Record<'_>,
                ctx: Context<'_, S>,
            ) {
                let mut g = Grab::default();
                values.record(&mut g);
                if let (Some(v), Some(span)) = (g.0, ctx.span(id)) {
                    // Only run_phase spans carry a `Stored`; others (chat/tool) no-op.
                    if let Some(st) = span.extensions_mut().get_mut::<Stored>() {
                        st.0 = Some(v);
                    }
                }
            }
            fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
                let span = ctx.span(&id).unwrap();
                if span.name() != "run_phase" {
                    return;
                }
                let v = span.extensions().get::<Stored>().and_then(|s| s.0.clone());
                self.0.lock().unwrap().push(v);
            }
        }

        // A driver that delegates one sweep then answers; the sweep returns a report.
        // Same shape as the request-shaping tests — the mock ignores the blob's content,
        // we only assert what the span recorded.
        fn client() -> ScriptedClient {
            const SYNTH: &str = "gemini-3.5-flash";
            const EXPLORER: &str = "gemini-flash-lite-latest";
            ScriptedClient::builder()
                .on_model(SYNTH, |req| {
                    if transcript_text(req).contains("REPORT") {
                        Ok(text_response("ANSWER"))
                    } else {
                        Ok(tool_call_response("s1", "explore", json!({ "question": "q" })))
                    }
                })
                .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
                .build()
        }
        const SYNTH: &str = "gemini-3.5-flash";
        const EXPLORER: &str = "gemini-flash-lite-latest";

        // On-theme: the Gemini 3-line thinkingLevel blob — the very shape PR 80 routes.
        let expected =
            thinking_params(ProviderKind::Gemini, SYNTH, 8192).expect("gemini sends params");
        let expected_str = expected.to_string();
        assert!(
            expected_str.contains("thinkingLevel"),
            "fixture must be the level blob, got {expected_str}"
        );

        // Positive: both arms carry the blob → every run_phase span records it.
        let cap = PhaseCapture::default();
        serialized_capture(async {
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);
            let cl = client();
            let dir = project_with_marker();
            consult_with(
                "q",
                dir.path(),
                &arm_with(&cl, EXPLORER, Some(expected.clone())),
                &arm_with(&cl, SYNTH, Some(expected.clone())),
                &ConsultConfig::default(),
            )
            .await
            .unwrap();
        });
        let seen = cap.0.lock().unwrap().clone();
        assert!(
            seen.len() >= 2,
            "both the driver and its sweep must emit a run_phase span, got {seen:?}"
        );
        assert!(
            seen.iter().all(|v| v.as_deref() == Some(expected_str.as_str())),
            "every run_phase span must record the exact thinking blob, got {seen:?}"
        );

        // Negative: no params → the field stays Empty → recorded as absent.
        let cap = PhaseCapture::default();
        serialized_capture(async {
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);
            let cl = client();
            let dir = project_with_marker();
            consult_with(
                "q",
                dir.path(),
                &arm(&cl, EXPLORER),
                &arm(&cl, SYNTH),
                &ConsultConfig::default(),
            )
            .await
            .unwrap();
        });
        let seen = cap.0.lock().unwrap().clone();
        assert!(!seen.is_empty(), "run_phase spans must still have fired");
        assert!(
            seen.iter().all(|v| v.is_none()),
            "a toggle-less phase must record no thinking blob, got {seen:?}"
        );
    }

    /// The per-phase payoff: when a cast's synth and explorer straddle a thinking-shape
    /// boundary, each request must carry the shape fit to *its own* model — never one
    /// resolved once and shared. Here an Anthropic-adaptive driver (top-level
    /// `thinking:{type:adaptive}` + `output_config.effort`) runs beside a Gemini-level
    /// explorer (nested `generationConfig.thinkingConfig.thinkingLevel`): structurally
    /// disjoint blobs, so a regression that shared one arm's params would land the wrong
    /// keys on the other's request and this test would catch it. (A cross-*provider*
    /// straddle now that Gemini is single-tier — the whole 3-line takes a level.)
    #[tokio::test]
    async fn each_phase_gets_thinking_fit_to_its_own_model() {
        const SYNTH: &str = "claude-sonnet-4-6"; // Anthropic adaptive → output_config.effort
        const EXPLORER: &str = "gemini-flash-lite-latest"; // Gemini → thinkingLevel

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("REPORT") {
                    Ok(text_response("ANSWER"))
                } else {
                    Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();

        consult_with(
            "q",
            dir.path(),
            &arm_with(
                &client,
                EXPLORER,
                thinking_params(ProviderKind::Gemini, EXPLORER, 4096),
            ),
            &arm_with(
                &client,
                SYNTH,
                thinking_params(ProviderKind::Anthropic, SYNTH, 4096),
            ),
            &cfg,
        )
        .await
        .unwrap();

        let params =
            |r: &crate::test_support::RecordedRequest| r.additional_params.as_ref().unwrap().clone();
        // The adaptive synth: top-level adaptive thinking + effort, no Gemini nesting.
        for r in client.requests_for(SYNTH) {
            let p = params(&r);
            assert_eq!(
                p["thinking"]["type"], "adaptive",
                "adaptive driver wants adaptive thinking"
            );
            assert_eq!(
                p["output_config"]["effort"], "high",
                "adaptive depth rides output_config.effort"
            );
            assert!(
                p.get("generationConfig").is_none(),
                "the Gemini nesting must not leak onto the Anthropic arm"
            );
        }
        // The Gemini explorer: nested thinkingLevel, none of the adaptive keys.
        for r in client.requests_for(EXPLORER) {
            let p = params(&r);
            assert_eq!(
                p["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high",
                "the Gemini sweep wants a level"
            );
            assert!(
                p.get("thinking").is_none() && p.get("output_config").is_none(),
                "the adaptive keys must not leak onto the Gemini arm"
            );
        }
    }

    /// The mixed-cast payoff: each phase runs on its OWN client. Two distinct
    /// scripted clients — the synth's knows only the synth model, the explorer's
    /// only the explorer model — so any cross-routing panics ("no responder").
    /// Each arm also carries its own `max_tokens`, and every request must show
    /// its own arm's value: the per-arm resolution the cast split exists for.
    #[tokio::test]
    async fn a_mixed_cast_routes_each_phase_to_its_own_client() {
        const SYNTH: &str = "claude-synth";
        const EXPLORER: &str = "deepseek-explorer";

        let synth_client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if transcript_text(req).contains("REPORT") {
                    Ok(text_response("ANSWER from the other wire"))
                } else {
                    Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                }
            })
            .build();
        let explorer_client = ScriptedClient::builder()
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig::default();
        let explorer_arm = Arm::new(
            explorer_client.clone(),
            EXPLORER,
            4096,
            None,
            ModelCaps {
                vision: false,
                tool_result_images: true,
            },
        );
        let synth_arm = Arm::new(
            synth_client.clone(),
            SYNTH,
            32768,
            None,
            ModelCaps {
                vision: true,
                tool_result_images: true,
            },
        );

        let out = consult_with("q", dir.path(), &explorer_arm, &synth_arm, &cfg)
            .await
            .expect("mixed-cast consult should succeed");

        assert!(out.answer.contains("ANSWER from the other wire"));
        assert!(out.report.contains("REPORT"), "the sweep crossed clients");
        // Routing held: each client saw only its own phase…
        assert!(!synth_client.requests_for(SYNTH).is_empty());
        assert!(!explorer_client.requests_for(EXPLORER).is_empty());
        assert!(
            synth_client.requests_for(EXPLORER).is_empty(),
            "the synth client must never serve the explorer model"
        );
        assert!(
            explorer_client.requests_for(SYNTH).is_empty(),
            "the explorer client must never serve the synth model"
        );
        // …and each request carried ITS arm's max_tokens, not a shared value.
        for r in synth_client.requests() {
            assert_eq!(
                r.max_tokens,
                Some(32768),
                "synth arm budget on {:?}",
                r.model
            );
        }
        for r in explorer_client.requests() {
            assert_eq!(
                r.max_tokens,
                Some(4096),
                "explorer arm budget on {:?}",
                r.model
            );
        }
    }

    /// `Arm::from_slot`, offline: the keyless openai kind builds with no network
    /// and no key, taking the slot's tunables (with per-role fallback), the
    /// caps pin, and the model id. The openai shape sends sampling but no
    /// thinking block.
    #[test]
    fn arm_from_slot_resolves_slot_tunables_and_caps() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "local".into(),
            kind: ProviderKind::Openai,
            base_url: Some("http://localhost:13305/api/v1".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot {
            vision: Some(true),
            max_tokens: Some(2048),
            temperature: Some(0.7),
            ..ModelSlot::bare("local", "llava-someday")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("keyless openai arm builds offline");
        assert_eq!(arm.model, "llava-someday");
        assert_eq!(arm.max_tokens, 2048, "slot max_tokens override wins");
        assert!(arm.caps.vision, "the vision pin survives into the arm");
        let params = arm.params.expect("openai sends sampling");
        assert_eq!(params["temperature"], 0.7, "slot temperature override wins");
        assert_eq!(params["top_p"], defaults.top_p);
        assert!(
            params.get("thinking").is_none(),
            "openai kind has no thinking toggle"
        );

        // The bare slot falls back to the per-role defaults.
        let bare = ModelSlot::bare("local", "m");
        let arm = Arm::from_slot(&backend, &bare, ModelRole::Explorer, &defaults).unwrap();
        assert_eq!(arm.max_tokens, defaults.max_tokens);
        assert!(!arm.caps.vision, "openai kind classifies blind by default");
        assert_eq!(
            arm.params.unwrap()["temperature"],
            defaults.explorer_temperature,
            "explorer role takes the explorer-side default"
        );
    }

    /// A hosted OpenAI Platform backend is still configured as kind `openai`, but
    /// it is not the same request shape as `openai-local`: current GPT reasoning
    /// models need rig's Responses client, where effort lives in
    /// `additional_params`, output budget maps to `max_output_tokens`, and
    /// sampling is model-aware. This pins the arm-level split so a future cleanup
    /// does not route hosted GPT back through the local-compatible Chat Completions
    /// path that sends `max_tokens`.
    #[test]
    fn hosted_openai_arm_uses_responses_shape() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "gpt".into(),
            kind: ProviderKind::Openai,
            base_url: Some(crate::credentials::HOSTED_OPENAI_BASE_URL.into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        assert!(
            backend.is_hosted_openai(),
            "fixture must select the hosted seam"
        );
        let slot = ModelSlot {
            vision: Some(true),
            temperature: Some(0.4),
            effort: Some("xhigh".into()),
            ..ModelSlot::bare("gpt", "gpt-5.6-sol")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("hosted OpenAI arm builds offline with a placeholder key");
        assert_eq!(arm.model, "gpt-5.6-sol");
        assert_eq!(
            arm.temperature, None,
            "current GPT reasoning models reject custom temperature"
        );
        let params = arm.params.expect("hosted OpenAI sends Responses params");
        assert_eq!(
            params["reasoning"]["effort"], "xhigh",
            "the slot effort reaches hosted OpenAI reasoning"
        );
        assert!(
            params.get("temperature").is_none() && params.get("top_p").is_none(),
            "current GPT reasoning models get no sampling knobs"
        );
        assert!(
            params.get("max_tokens").is_none() && params.get("max_completion_tokens").is_none(),
            "rig's Responses client maps core max_tokens to max_output_tokens"
        );
        assert!(
            arm.caps.vision,
            "the vision pin survives into the hosted arm"
        );
    }

    /// An effort rig's typed request builder can't name fails **here**, at arm
    /// construction, with a message an operator can act on — not mid-consult as a bare
    /// `unknown variant \`max\``.
    ///
    /// Two wires parse kaibo's blob into a typed struct with a closed rung set (rig's
    /// Gemini `ThinkingLevel`, its Responses `ReasoningEffort`), and that ceiling is
    /// rig's client rather than the provider's API — OpenAI's own endpoint takes `"max"`.
    /// kaibo keeps no allowlist of its own, so the refusal comes from asking rig's
    /// converter, and the rungs it offers are read back out of rig too
    /// (`tests/effort_wire.rs` pins both against real request bodies). What this test
    /// owns is the *message*: the model, the backend, the role, the value that failed,
    /// whose ceiling it is, and what to type instead.
    #[test]
    fn a_rig_refused_effort_fails_at_arm_construction_with_an_actionable_message() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "google".into(),
            kind: ProviderKind::Gemini,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        // `docs/config.example.toml` used to invite exactly this, and it broke every
        // Gemini cast the moment a call was made.
        let slot = ModelSlot {
            effort: Some("xhigh".into()),
            ..ModelSlot::bare("google", "gemini-3.5-flash")
        };
        let err = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect_err("rig's Gemini builder refuses xhigh")
            .to_string();
        assert!(err.contains("gemini-3.5-flash"), "names the model: {err}");
        assert!(err.contains("google"), "names the backend: {err}");
        assert!(err.contains("synth"), "names the role: {err}");
        assert!(err.contains("xhigh"), "names the value that failed: {err}");
        assert!(
            err.contains("rig-core"),
            "says whose ceiling this is, so the operator doesn't go hunting in \
             Google's docs for a limit that isn't theirs: {err}"
        );
        assert!(
            err.contains("minimal | low | medium | high"),
            "offers the rungs this wire does take: {err}"
        );

        // A rung the wire accepts builds normally — the preflight adds no ceiling of
        // its own, it only reports rig's.
        let slot = ModelSlot {
            effort: Some("low".into()),
            ..ModelSlot::bare("google", "gemini-3.5-flash")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("an accepted rung builds offline with a placeholder key");
        assert_eq!(
            arm.params.expect("gemini sends a thinking block")["generationConfig"]
                ["thinkingConfig"]["thinkingLevel"],
            "low"
        );

        // And a passthrough wire keeps passing anything through — the preflight must not
        // quietly become a global allowlist.
        let backend = crate::config::Backend {
            name: "deepseek".into(),
            kind: ProviderKind::DeepSeek,
            ..backend
        };
        let slot = ModelSlot {
            effort: Some("ludicrous".into()),
            ..ModelSlot::bare("deepseek", "deepseek-v4-pro")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a passthrough wire carries an unknown rung to the provider");
        assert_eq!(
            arm.params.expect("deepseek sends a thinking block")["reasoning_effort"],
            "ludicrous"
        );
    }

    /// An OpenAI-compatible gateway that faithfully proxies `/v1/responses` (verified
    /// against a real gateway) is not OpenAI Platform's own endpoint, so the
    /// endpoint-exact heuristic alone would leave it on Chat Completions — starving
    /// current GPT-5.x reasoning models, which reject `max_tokens` outright. An
    /// explicit `wire = "responses"` on the backend opts it into the same
    /// Responses-shaped arm hosted OpenAI Platform gets, without the gateway needing
    /// to sit at Platform's exact URL.
    #[test]
    fn responses_wire_gateway_arm_uses_responses_shape() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "gateway".into(),
            kind: ProviderKind::Openai,
            base_url: Some("https://llm-gateway.example.internal/v1".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: Some(crate::config::OpenaiWire::Responses),
        };
        assert!(
            !backend.is_hosted_openai(),
            "fixture must NOT be OpenAI Platform itself — that's the whole point"
        );
        assert!(
            backend.uses_responses_wire(),
            "the explicit wire override selects the Responses shape"
        );
        let slot = ModelSlot {
            effort: Some("high".into()),
            ..ModelSlot::bare("gateway", "gpt-5.6-sol")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a responses-wire gateway arm builds offline with a placeholder key");
        assert_eq!(arm.model, "gpt-5.6-sol");
        let params = arm.params.expect("responses-wire gateway sends Responses params");
        assert_eq!(
            params["reasoning"]["effort"], "high",
            "reasoning effort reaches the gateway the same way it reaches Platform"
        );
        assert!(
            params.get("max_tokens").is_none() && params.get("max_completion_tokens").is_none(),
            "the Responses client maps core max_tokens to max_output_tokens, not \
             the Chat Completions field GPT-5.x rejects"
        );
    }

    /// Older hosted GPT chat models keep sampling: the same Responses seam is used,
    /// but model-aware shaping routes temperature through rig's typed field and
    /// `top_p` through Responses additional parameters.
    #[test]
    fn hosted_openai_arm_keeps_sampling_for_compatible_gpt_models() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "gpt".into(),
            kind: ProviderKind::Openai,
            base_url: Some(crate::credentials::HOSTED_OPENAI_BASE_URL.into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot {
            temperature: Some(0.4),
            ..ModelSlot::bare("gpt", "gpt-4.1-mini")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("hosted OpenAI GPT-4.1 arm builds offline");
        assert_eq!(
            arm.temperature,
            Some(0.4),
            "sampling-compatible GPT models keep typed temperature"
        );
        let params = arm.params.expect("hosted OpenAI sends Responses params");
        assert!(
            params.get("reasoning").is_none(),
            "GPT-4.1 rejected reasoning.effort in the live probe"
        );
        assert_eq!(
            params["top_p"], defaults.top_p,
            "sampling-compatible GPT models keep top_p"
        );
    }

    /// An anthropic-kind backend pointed at a custom `base_url` (an Anthropic-
    /// Messages-API-compatible gateway/proxy, auth via network identity rather than
    /// a real key) must still build offline, exactly like the keyless openai arm
    /// above — proving the conditional `.base_url(...)` call in the Anthropic arm
    /// of `Arm::from_slot` doesn't disturb the builder chain (the ordering a
    /// cross-family review flagged for a closer look).
    #[test]
    fn arm_from_slot_builds_an_anthropic_arm_with_a_custom_base_url() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: Some("https://gateway.example.ts.net".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot::bare("anthropic", "claude-sonnet-4-6");
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyless anthropic arm with a custom base_url builds offline");
        assert_eq!(arm.model, "claude-sonnet-4-6");

        // An anthropic backend with no base_url must still build (the default
        // rig endpoint applies) — the conditional branch must not be required.
        let default_backend = crate::config::Backend {
            base_url: None,
            ..backend
        };
        Arm::from_slot(&default_backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyless anthropic arm with no base_url still builds offline");
    }

    /// A gemini-kind backend pointed at a custom `base_url` (a Gemini-API-compatible
    /// gateway/proxy in front of the real backend) must build offline too — the
    /// same conditional `.base_url(...)` pattern as the Anthropic arm above, now on
    /// the Gemini arm of `Arm::from_slot`. The contract is a HOST ROOT (mirroring
    /// rig's own Gemini `ClientBuilder`, which appends its own `/v1beta/...` path).
    #[test]
    fn arm_from_slot_builds_a_gemini_arm_with_a_custom_base_url() {
        let defaults = crate::config::Defaults::default();
        let backend = crate::config::Backend {
            name: "gemini".into(),
            kind: ProviderKind::Gemini,
            base_url: Some("https://llm-gateway.example.internal".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot::bare("gemini", "gemini-3.5-flash");
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyless gemini arm with a custom base_url builds offline");
        assert_eq!(arm.model, "gemini-3.5-flash");

        // A gemini backend with no base_url must still build (the default rig
        // endpoint applies) — the conditional branch must not be required.
        let default_backend = crate::config::Backend {
            base_url: None,
            ..backend
        };
        Arm::from_slot(&default_backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyless gemini arm with no base_url still builds offline");
    }

    /// The OpenRouter arm's full params assembly — `to_params` chained through
    /// `inject_output_budget` at the single live construction point. The reasoning
    /// object (thinking on by default), the rig-defect budget workaround, and the
    /// slot's sampling must coexist in the one blob the arm sends; any of them
    /// silently missing starves or blinds the call. Keyed via a key file so the
    /// test never touches process env.
    #[test]
    fn openrouter_arm_carries_reasoning_budget_and_sampling_together() {
        let defaults = crate::config::Defaults::default();
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("openrouter-key");
        std::fs::write(&key_file, "sk-or-test").unwrap();
        let backend = crate::config::Backend {
            name: "openrouter".into(),
            kind: ProviderKind::OpenRouter,
            base_url: None,
            api_key_env: None,
            api_key_file: Some(key_file.to_str().unwrap().to_string()),
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot {
            temperature: Some(0.3),
            ..ModelSlot::bare("openrouter", "~anthropic/claude-sonnet-latest")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyed openrouter arm builds from a key file");
        assert_eq!(arm.model, "~anthropic/claude-sonnet-latest");
        let params = arm.params.expect("the openrouter arm always sends params");
        assert_eq!(
            params["reasoning"]["effort"],
            defaults.synth_effort.as_str(),
            "reasoning rides on by default at the synth-role effort"
        );
        assert_eq!(
            params["max_completion_tokens"], defaults.max_tokens,
            "the output budget must reach the body rig won't carry natively"
        );
        assert_eq!(params["temperature"], 0.3, "slot sampling coexists");
        assert_eq!(
            params["provider"]["data_collection"], "deny",
            "no-collection routing rides every OpenRouter arm by default — source \
             must not reach a data-collecting upstream host without an explicit opt-in"
        );
    }

    /// The explicit `data_collection = "allow"` opt-in threads all the way to the
    /// arm: the provider pin must be *absent* (kaibo steps aside for the account's
    /// own settings — it never emits a restriction it was told to drop, and never
    /// pushes toward collection with an explicit "allow"). Arm-level on purpose
    /// (GLM review, 2026-07-03): a flipped condition in `inject_provider_prefs`
    /// would slip past the function-level test alone.
    #[test]
    fn openrouter_arm_allow_omits_the_provider_pin() {
        let defaults = crate::config::Defaults::default();
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("openrouter-key");
        std::fs::write(&key_file, "sk-or-test").unwrap();
        let backend = crate::config::Backend {
            name: "openrouter".into(),
            kind: ProviderKind::OpenRouter,
            base_url: None,
            api_key_env: None,
            api_key_file: Some(key_file.to_str().unwrap().to_string()),
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: crate::config::DataCollection::Allow,
            wire: None,
        };
        let slot = ModelSlot::bare("openrouter", "~anthropic/claude-sonnet-latest");
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyed openrouter arm builds from a key file");
        let params = arm.params.expect("the openrouter arm always sends params");
        assert!(
            params.get("provider").is_none(),
            "the opt-in must reach the arm as an *absent* pin, got: {params}"
        );
        assert_eq!(
            params["reasoning"]["effort"],
            defaults.synth_effort.as_str(),
            "everything else in the blob is unchanged by the opt-in"
        );
    }

    /// `effort = "none"` on a slot threads to the arm as the structural reasoning
    /// disable, coexisting with the budget workaround and the privacy pin — the
    /// full injection chain, not just the `to_params` step it's pinned at in
    /// shaping.rs (GLM review, 2026-07-03).
    #[test]
    fn openrouter_arm_effort_none_carries_the_structural_disable() {
        let defaults = crate::config::Defaults::default();
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("openrouter-key");
        std::fs::write(&key_file, "sk-or-test").unwrap();
        let backend = crate::config::Backend {
            name: "openrouter".into(),
            kind: ProviderKind::OpenRouter,
            base_url: None,
            api_key_env: None,
            api_key_file: Some(key_file.to_str().unwrap().to_string()),
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot {
            effort: Some("none".into()),
            ..ModelSlot::bare("openrouter", "z-ai/glm-5.2")
        };
        let arm = Arm::from_slot(&backend, &slot, ModelRole::Synth, &defaults)
            .expect("a keyed openrouter arm builds from a key file");
        let params = arm.params.expect("the openrouter arm always sends params");
        assert_eq!(
            params["reasoning"]["enabled"], false,
            "the opt-out must arrive structurally, not as a passthrough string"
        );
        assert!(
            params["reasoning"].get("effort").is_none(),
            "no effort string rides beside the disable, got: {params}"
        );
        assert_eq!(
            params["max_completion_tokens"], defaults.max_tokens,
            "the budget workaround coexists with the disable"
        );
        assert_eq!(
            params["provider"]["data_collection"], "deny",
            "the privacy pin coexists with the disable"
        );
    }

    /// The OpenRouter arm rides with rig's explicit prompt caching ON: Anthropic-
    /// upstream slugs need `cache_control` breakpoints to bill cache-read rates
    /// (2026-07-03: a consult re-billed its full growing prefix every turn without
    /// them), and implicit-caching upstreams ignore the marker. `from_slot` routes
    /// through this constructor — so this pins the wiring, not an internal default.
    ///
    /// **Asserted on the wire, not on a flag.** What matters is the serialized request
    /// body, so that is what a fake transport captures here: rig could keep
    /// `with_prompt_caching()` compiling while changing what it emits, and any check
    /// short of the body would stay green through that.
    #[tokio::test]
    async fn openrouter_arm_enables_prompt_caching() {
        async fn system_block(model: openrouter::CompletionModel<CaptureHttp>, http: &CaptureHttp) -> Value {
            let request = CompletionRequest {
                model: None,
                preamble: Some("ground every claim".into()),
                chat_history: OneOrMany::one(Message::user("hi")),
                documents: Vec::new(),
                tools: Vec::new(),
                temperature: None,
                max_tokens: Some(4096),
                tool_choice: None,
                additional_params: None,
                output_schema: None,
                record_telemetry_content: false,
            };
            // The capture transport always errors; the body is already built by then.
            let _ = model.completion(request).await;
            http.recorded()
                .expect("rig serialized a request body")
                .pointer("/messages/0")
                .cloned()
                .expect("the system message leads the body")
        }

        let http = CaptureHttp::default();
        let client = openrouter::Client::builder()
            .api_key("sk-or-test")
            .http_client(http.clone())
            .build()
            .expect("offline openrouter client construction");

        let plain_http = CaptureHttp::default();
        let plain_client = openrouter::Client::builder()
            .api_key("sk-or-test")
            .http_client(plain_http.clone())
            .build()
            .expect("offline openrouter client construction");
        let plain = system_block(
            plain_client.completion_model("~anthropic/claude-sonnet-latest"),
            &plain_http,
        )
        .await;
        assert!(
            !plain.to_string().contains("cache_control"),
            "rig marks no breakpoint by default — the arm constructor is load-bearing: \
             {plain}"
        );

        let armed = system_block(
            Arm::openrouter_completion_model(&client, "~anthropic/claude-sonnet-latest"),
            &http,
        )
        .await;
        assert!(
            armed.to_string().contains(r#""cache_control""#),
            "the OpenRouter arm must put a cache breakpoint on the system prompt: {armed}"
        );
        assert!(
            armed.to_string().contains("ephemeral"),
            "the breakpoint is the ephemeral kind OpenRouter bills against: {armed}"
        );
    }

    /// `Arm::from_slot` is the single live construction point, and per-call
    /// overrides build slots that never saw config load's budget validation —
    /// so the thinking_budget < max_tokens rule must hold here too, as the same
    /// keyworded boundary error, not a provider 400 mid-call. Validated before
    /// key resolution, so it fires with no key configured.
    #[test]
    fn arm_from_slot_rejects_an_inverted_thinking_budget() {
        let defaults = crate::config::Defaults {
            max_tokens: 4096,
            thinking_budget: 8192, // inverted vs max_tokens
            ..crate::config::Defaults::default()
        };
        let backend = crate::config::Backend {
            name: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot::bare("anthropic", "claude-haiku-4-5");
        let err = Arm::from_slot(&backend, &slot, ModelRole::Explorer, &defaults)
            .expect_err("an inverted budget must be caught at arm construction");
        let msg = format!("{err:#}");
        assert!(msg.contains("thinking_budget"), "got: {msg}");
        assert!(msg.contains("max_tokens"), "got: {msg}");
    }

    /// The mirror: a shape with no budget sink (Gemini takes a `thinkingLevel`, not a
    /// budget) carries an inert `thinking_budget`, so an inverted pair can't starve the
    /// answer and must not be refused here. The arm may still fail later on key
    /// resolution (no key configured), but never with the inverted-budget error — that
    /// distinguishes the fix from the pre-gate behavior, which refused Gemini the same
    /// way it refuses a budget-tier Anthropic slot.
    #[test]
    fn arm_from_slot_accepts_an_inverted_budget_with_no_budget_sink() {
        let defaults = crate::config::Defaults {
            max_tokens: 4096,
            thinking_budget: 8192, // inverted vs max_tokens, but inert for Gemini
            ..crate::config::Defaults::default()
        };
        let backend = crate::config::Backend {
            name: "gemini".into(),
            kind: ProviderKind::Gemini,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
            wire: None,
        };
        let slot = ModelSlot::bare("gemini", "gemini-3.5-flash");
        if let Err(e) = Arm::from_slot(&backend, &slot, ModelRole::Explorer, &defaults) {
            let msg = format!("{e:#}");
            assert!(
                !msg.contains("thinking_budget"),
                "Gemini carries no budget; the inverted pair must not be refused: {msg}"
            );
        }
    }

    /// `run_phase` builds the toolset twice — once for the main loop, again for the
    /// forced finalize turn. The `reports` sink is created once in `consult_with` and
    /// `clone()`d into each build, so a sweep that completed before the cap must
    /// survive into the final `ConsultOutput.report`. Delegate once, then burn turns
    /// on `run_kaish` until the cap forces a finalize (the second build) — the first
    /// sweep's report must still be there.
    #[tokio::test]
    async fn a_sweeps_report_survives_the_finalize_toolset_rebuild() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if is_finalize_turn(req) {
                    Ok(text_response("FINAL"))
                } else if !transcript_text(req).contains("REPORT-E") {
                    // First turn: delegate one sweep (pushes to the shared sink).
                    Ok(tool_call_response(
                        "s1",
                        "explore",
                        json!({ "question": "find it" }),
                    ))
                } else {
                    // Already swept; keep burning turns without re-delegating, so the
                    // cap fires and forces the second toolset build.
                    Ok(tool_call_response(
                        "k",
                        "run_kaish",
                        json!({ "script": "cat src/foo.rs" }),
                    ))
                }
            })
            .on_model(EXPLORER, |_req| Ok(text_response("REPORT-E: src/foo.rs:1")))
            .build();

        let dir = project_with_marker();
        let cfg = ConsultConfig {
            synth_max_turns: 2,
            ..ConsultConfig::default()
        };

        let out = consult_with(
            "q",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        // The teeth: were `reports` rebuilt per `make_tools` call instead of shared,
        // the pre-cap sweep would be lost and this would be empty.
        assert!(
            out.report.contains("REPORT-E"),
            "the pre-cap sweep's report must survive the finalize rebuild, got: {:?}",
            out.report
        );
        assert_eq!(
            client.requests_for(EXPLORER).len(),
            1,
            "exactly one sweep was delegated (the rest burned run_kaish)"
        );
        assert!(
            client
                .requests_for(SYNTH)
                .iter()
                .any(|r| r.tool_choice == Some(ToolChoice::None)),
            "the cap must have forced a finalize turn (the second toolset build)"
        );
        assert!(out.answer.contains("FINAL"), "finalize produced the answer");
    }

    /// Session glue, end to end and offline: a second turn must *see* the first
    /// turn's `(question, answer)` pair in its prompt, and both turns must accumulate
    /// in the store. This is the `server.consult` history→consult→record dance,
    /// now `consult_session_turn`, driven by a mock — the seam the live `#[ignore]`d
    /// tests couldn't pin without a real model.
    #[tokio::test]
    async fn a_second_turn_replays_the_first_turns_pair_and_records() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        let sid = "thread-1";

        // Turn 1.
        let out1 = consult_session_turn(
            Some((&sessions, sid)),
            "Q1 what is kaish",
            None,
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(out1.answer, "ANSWER[Q1 what is kaish]");
        assert_eq!(
            sessions.history(sid).await.unwrap(),
            vec![QaTurn::new("Q1 what is kaish", "ANSWER[Q1 what is kaish]")],
            "turn 1 must be recorded"
        );

        // Turn 2.
        let out2 = consult_session_turn(
            Some((&sessions, sid)),
            "Q2 who calls it",
            None,
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        // The teeth: turn 2's request carried turn 1's Q and A into the prompt.
        let turn2_req = &client.requests_for(SYNTH)[1];
        assert!(
            turn2_req.user_text.contains("Q1 what is kaish"),
            "turn 2 must replay turn 1's question: {:?}",
            turn2_req.user_text
        );
        assert!(
            turn2_req.user_text.contains("ANSWER[Q1 what is kaish]"),
            "turn 2 must replay turn 1's answer: {:?}",
            turn2_req.user_text
        );
        assert_eq!(out2.answer, "ANSWER[Q2 who calls it]");
        assert_eq!(
            sessions.history(sid).await.unwrap().len(),
            2,
            "both turns accumulate in the thread"
        );
    }

    /// The same replay→record dance, but over the **durable** seam backed by a tempfile
    /// turso db — proving `consult_session_turn` works byte-for-byte against the persistent
    /// store the server uses when `[persistence]` is on. A reopen of the same db (a
    /// simulated restart) still shows both turns, so the thread survives a process restart.
    #[tokio::test]
    async fn store_backed_session_replays_records_and_survives_reopen() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let dir = tempdir().unwrap();
        let sessions = persistent_store(dir.path()).await;
        let cfg = ConsultConfig::default();
        let sid = "durable-thread";

        // Turn 1 then turn 2, exactly as the in-memory test.
        for q in ["Q1 durable", "Q2 durable"] {
            consult_session_turn(
                Some((&sessions, sid)),
                q,
                None,
                dir.path(),
                &arm(&client, "explorer"),
                &arm(&client, SYNTH),
                &cfg,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            sessions.history(sid).await.unwrap(),
            vec![
                QaTurn::new("Q1 durable", "ANSWER[Q1 durable]"),
                QaTurn::new("Q2 durable", "ANSWER[Q2 durable]"),
            ],
            "both turns accumulate in the durable thread"
        );

        // Simulated restart: drop the store handle, reopen the same db file. The thread
        // must still be there — the point of persistence.
        drop(sessions);
        let reopened = persistent_store(dir.path()).await;
        assert_eq!(
            reopened.history(sid).await.unwrap().len(),
            2,
            "the thread must survive a store reopen (process restart)"
        );
    }

    /// A failed turn must NOT record — a half-answer can't be allowed to poison the
    /// thread as established context the next turn would trust. (The invariant the
    /// `server.rs:325` comment used to assert only in prose.)
    #[tokio::test]
    async fn a_failed_turn_does_not_record() {
        const SYNTH: &str = "synth";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |_req| Err(provider_error("scripted failure")))
            .build();
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        let sid = "doomed";

        let result = consult_session_turn(
            Some((&sessions, sid)),
            "Q that fails",
            None,
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await;

        assert!(
            result.is_err(),
            "a provider error must surface, not be swallowed"
        );
        assert!(
            sessions.history(sid).await.unwrap().is_empty(),
            "a failed turn must leave the thread untouched, got: {:?}",
            sessions.history(sid).await.unwrap()
        );
    }

    /// Finding 3 (Gemini review): a *record* failure is non-fatal — by then the model has
    /// answered (a paid-for result), so the turn returns that answer rather than throwing it
    /// away over a bookkeeping write. The failure is surfaced LOUDLY on the result (a visible
    /// line), never a silent drop. (History failure stays fatal; that's the `?` above.)
    #[tokio::test]
    async fn a_record_failure_returns_the_answer_and_surfaces_the_failure() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let sessions = Sessions::FailingRecord;
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();

        let out = consult_session_turn(
            Some((&sessions, "doomed-record")),
            "Q that answers",
            None,
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("a record failure must NOT fail the turn — the answer is already paid for");

        assert!(
            out.answer.contains("ANSWER[Q that answers]"),
            "the paid-for answer must be preserved: {:?}",
            out.answer
        );
        // The record failure rides on `out.warnings`, NOT the answer text — so a machine
        // consumer of the answer gets the model's words uncorrupted (the #77 Gemini fix).
        assert!(
            !out.answer.contains("NOT recorded"),
            "the answer text must stay clean of kaibo's injected notices: {:?}",
            out.answer
        );
        assert!(
            out.warnings.iter().any(|w| w.contains("NOT recorded")),
            "the record failure must be surfaced loudly on out.warnings: {:?}",
            out.warnings
        );
    }

    /// A stateless turn (`session: None`) records nothing and replays nothing — the
    /// one-shot path stays byte-for-byte its pre-session self.
    #[tokio::test]
    async fn a_stateless_turn_records_nothing() {
        const SYNTH: &str = "synth";
        let client = echo_client(SYNTH);
        let sessions = store();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();

        let out = consult_session_turn(
            None,
            "lone question",
            None,
            dir.path(),
            &arm(&client, "explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .unwrap();

        assert_eq!(out.answer, "ANSWER[lone question]");
        assert_eq!(
            sessions.session_count().await.unwrap(),
            0,
            "a stateless turn creates no session"
        );
    }

    /// The recomposed consult must drive BOTH tools: a direct `run_kaish` and the
    /// delegated `explore′`. Pin the wiring offline — no model, just the toolset.
    /// A non-vision synth gets no `view_image` (it's gated on the synth's caps).
    #[test]
    fn consult_toolset_has_both_run_kaish_and_explore() {
        let dir = tempdir().unwrap();
        // The scripted client satisfies the same trait bounds with no network and no
        // key-format requirement — so this stays a pure toolset-wiring test, not a
        // hostage to rig's anthropic constructor.
        let client = ScriptedClient::builder().build();
        let cfg = ConsultConfig::default();
        let reports = Arc::new(Mutex::new(Vec::new()));

        let synth = arm(&client, "synth-model"); // not vision-capable
        let tools = consult_tools(
            &arm(&client, "explorer-model"),
            dir.path(),
            &cfg,
            reports,
            Arc::new(Mutex::new(Usage::new())),
            &synth,
        )
        .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        assert!(
            names.iter().any(|n| n == "run_kaish"),
            "missing run_kaish, got {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "explore"),
            "missing explore′, got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "view_image"),
            "a blind synth must not get view_image, got {names:?}"
        );
    }

    /// When the synth arm is vision-capable, the consult driver's toolset gains
    /// `view_image` — the consumption of the resolved caps. The explorer arm's caps
    /// are irrelevant here (the driver runs on the synth); the bool models that.
    #[test]
    fn a_vision_synth_gets_view_image_in_the_consult_toolset() {
        let dir = tempdir().unwrap();
        let client = ScriptedClient::builder().build();
        let cfg = ConsultConfig::default();
        let reports = Arc::new(Mutex::new(Vec::new()));

        let synth = vision_arm(&client, "synth-model"); // synth IS vision-capable
        let tools = consult_tools(
            &arm(&client, "explorer-model"),
            dir.path(),
            &cfg,
            reports,
            Arc::new(Mutex::new(Usage::new())),
            &synth,
        )
        .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        assert!(
            names.iter().any(|n| n == "view_image"),
            "a vision synth must get view_image, got {names:?}"
        );
    }

    /// The `consult` driver's toolset: `run_kaish` and the nested `explore′` always,
    /// `view_image` exactly when the *synth* arm is vision-capable. Pins the gate both
    /// ways so the driver's perception cap doesn't drift.
    #[test]
    fn consult_tools_gate_view_image_on_the_synth_vision_cap() {
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();
        let client = ScriptedClient::builder()
            .on_model("m", |_r| Ok(text_response("x")))
            .build();
        let explorer = arm(&client, "m");
        let reports = Arc::new(Mutex::new(Vec::<String>::new()));

        let usage_sink = Arc::new(Mutex::new(Usage::new()));
        let blind_synth = arm(&client, "synth-model");
        let blind = consult_tools(
            &explorer,
            dir.path(),
            &cfg,
            reports.clone(),
            usage_sink.clone(),
            &blind_synth,
        )
        .expect("blind toolset builds");
        let blind_names: Vec<String> = blind.iter().map(|t| t.name().to_string()).collect();
        assert!(blind_names.iter().any(|n| n == "run_kaish"));
        assert!(blind_names.iter().any(|n| n == "explore"));
        assert!(
            !blind_names.iter().any(|n| n == "view_image"),
            "no view_image without vision, got {blind_names:?}"
        );

        let seeing_synth = vision_arm(&client, "synth-model");
        let seeing = consult_tools(&explorer, dir.path(), &cfg, reports, usage_sink, &seeing_synth)
            .expect("vision toolset builds");
        let seeing_names: Vec<String> = seeing.iter().map(|t| t.name().to_string()).collect();
        assert!(
            seeing_names.iter().any(|n| n == "view_image"),
            "view_image present with vision, got {seeing_names:?}"
        );
    }

    /// A `ConsultConfig` carrying an artifact sink over an in-memory store — the shape
    /// the server builds when all three keys hold.
    fn cfg_with_artifact_sink() -> (ConsultConfig, Arc<crate::artifact::ArtifactSink>) {
        let sink = Arc::new(crate::artifact::ArtifactSink::new(
            Arc::new(crate::cas::MediaStore::Memory(crate::cas::MemoryCas::new(
                None,
            ))),
            crate::artifact::ArtifactAuthor {
                prompt: "q".into(),
                model: "synth-model".into(),
                cast: "test".into(),
                slot: "synth",
                session: None,
            },
        ));
        let cfg = ConsultConfig {
            artifacts: Some(Arc::clone(&sink)),
            ..ConsultConfig::default()
        };
        (cfg, sink)
    }

    /// `save_artifact` reaches the driver's toolset exactly when the server built a sink
    /// for this call, and never otherwise. The `None` arm is the default posture of every
    /// kaibo install — the tool does not exist, so a driver cannot call it whatever it
    /// decides to try.
    #[test]
    fn save_artifact_is_in_the_driver_toolset_only_with_a_sink() {
        let dir = tempdir().unwrap();
        let client = ScriptedClient::builder().build();
        let explorer = arm(&client, "explorer-model");
        let synth = arm(&client, "synth-model");
        let names = |cfg: &ConsultConfig| -> Vec<String> {
            consult_tools(
                &explorer,
                dir.path(),
                cfg,
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Mutex::new(Usage::new())),
                &synth,
            )
            .expect("toolset builds")
            .iter()
            .map(|t| t.name().to_string())
            .collect()
        };

        let without = names(&ConsultConfig::default());
        assert!(
            !without.iter().any(|n| n == "save_artifact"),
            "no sink means no tool at all, got {without:?}"
        );

        let (cfg, _sink) = cfg_with_artifact_sink();
        let with = names(&cfg);
        assert!(
            with.iter().any(|n| n == "save_artifact"),
            "a sink puts the tool in the driver's toolset, got {with:?}"
        );
    }

    /// **A delegated sweep never gets `save_artifact`.** v1 is driver-loop only, and the
    /// placement enforces it: a sweep's toolset is built from the explore rung, which has
    /// no sink to read. Driven end to end on a real nested sweep so this pins the
    /// *running* toolset rather than a construction detail.
    #[tokio::test]
    async fn a_delegated_sweep_never_carries_save_artifact() {
        const SYNTH: &str = "capable-synth";
        const EXPLORER: &str = "cheap-explorer";
        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                assert!(
                    has_tool(req, "save_artifact"),
                    "the driver has the tool in this run, so the sweep's absence is a \
                     real contrast: {:?}",
                    req.tools
                );
                if !transcript_text(req).contains("SWEPT") {
                    Ok(tool_call_response(
                        "t1",
                        "explore",
                        json!({ "question": "what is here?" }),
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .on_model(EXPLORER, |req| {
                assert!(
                    !has_tool(req, "save_artifact"),
                    "a delegated sweep must never be handed save_artifact: {:?}",
                    req.tools
                );
                Ok(text_response("SWEPT: src/foo.rs:1"))
            })
            .build();

        let dir = project_with_marker();
        let (cfg, sink) = cfg_with_artifact_sink();
        let out = consult_with(
            "q",
            dir.path(),
            &arm(&client, EXPLORER),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");
        assert_eq!(out.answer, "ANSWER");
        assert!(
            sink.saved().is_empty(),
            "nothing was saved in this run, so nothing is in the ledger"
        );
    }

    /// **A consult that saves and then fails keeps the artifact.** The sink lives on the
    /// call, not the loop, so a provider error on a later turn cannot unwrite bytes that
    /// are already durable. This pins the half the engine owns; the server renders those
    /// digests into the failure text (see `consultation_failure_text_with_artifacts`), and
    /// without both halves a failed consult would silently orphan real content.
    #[tokio::test]
    async fn artifacts_saved_before_a_failure_survive_the_failure() {
        const SYNTH: &str = "capable-synth";
        const BODY: &str = "half the corpus\n";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("kaibo://cas/") {
                    Ok(tool_call_response(
                        "t-save",
                        "save_artifact",
                        json!({ "label": "partial corpus", "content": BODY }),
                    ))
                } else {
                    // Saved, then the provider falls over.
                    Err(provider_error("overloaded_error"))
                }
            })
            .on_model("cheap-explorer", |_r| Ok(text_response("unused")))
            .build();

        let dir = project_with_marker();
        let (cfg, sink) = cfg_with_artifact_sink();
        let err = consult_with(
            "make a corpus",
            dir.path(),
            &arm(&client, "cheap-explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect_err("the scripted provider fails after the save");
        assert!(
            format!("{err:#}").contains("overloaded"),
            "the failure is the provider's, got: {err:#}"
        );

        let saved = sink.saved();
        assert_eq!(saved.len(), 1, "the save happened and stands");
        assert_eq!(
            saved[0].digest,
            crate::cas::Digest::of_bytes(BODY.as_bytes()).to_hex()
        );
    }

    /// **The answer recorded into a session carries the artifact footer.**
    ///
    /// The digests are the only handle on bytes that outlive the call, and kaibo ships no
    /// GC, so where they get written down matters. The session turn is the natural place:
    /// it already sits in the state db beside the conversation that produced them, needs
    /// no schema, and a later turn replaying this thread sees what it saved. Recording the
    /// bare answer instead would leave the db holding a conversation about artifacts whose
    /// addresses it does not contain.
    #[tokio::test]
    async fn the_recorded_session_answer_carries_the_artifact_footer() {
        const SYNTH: &str = "capable-synth";
        const BODY: &str = "one\ntwo\nthree\n";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("kaibo://cas/") {
                    Ok(tool_call_response(
                        "t-save",
                        "save_artifact",
                        json!({ "label": "the inventory", "content": BODY }),
                    ))
                } else {
                    Ok(text_response("ANSWER: see the artifact."))
                }
            })
            .on_model("cheap-explorer", |_r| Ok(text_response("unused")))
            .build();

        let dir = project_with_marker();
        let (cfg, sink) = cfg_with_artifact_sink();
        let sessions = store();
        consult_session_turn(
            Some((&sessions, "s-1")),
            "make an inventory",
            None,
            dir.path(),
            &arm(&client, "cheap-explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let digest = sink.saved()[0].digest.clone();
        let history = sessions.history("s-1").await.expect("session history");
        assert_eq!(history.len(), 1, "one turn recorded");
        assert!(
            history[0].answer.contains(&format!("kaibo://cas/{digest}")),
            "the persisted answer must carry the digest, got: {:?}",
            history[0].answer
        );
    }

    /// The load-bearing offline e2e: a scripted driver calls `save_artifact` mid-loop,
    /// the bytes land in the store under their own digest, and the caller's footer names
    /// that digest by its resource URI. If the tool were wired into the toolset but its
    /// results never reached the caller, this is what would catch it.
    #[tokio::test]
    async fn a_driver_that_saves_an_artifact_reports_it_in_the_callers_footer() {
        const SYNTH: &str = "capable-synth";
        const CORPUS: &str = "cat -n /etc/passwd\ngrep -rn foo .\n";

        let client = ScriptedClient::builder()
            .on_model(SYNTH, |req| {
                if !transcript_text(req).contains("kaibo://cas/") {
                    Ok(tool_call_response(
                        "t-save",
                        "save_artifact",
                        json!({
                            "label": "the fuzz corpus",
                            "content": CORPUS,
                            "format": "text",
                        }),
                    ))
                } else {
                    Ok(text_response("ANSWER: the corpus is in the artifact."))
                }
            })
            .on_model("cheap-explorer", |_req| Ok(text_response("unused")))
            .build();

        let dir = project_with_marker();
        let (cfg, sink) = cfg_with_artifact_sink();
        let out = consult_with(
            "generate a fuzz corpus",
            dir.path(),
            &arm(&client, "cheap-explorer"),
            &arm(&client, SYNTH),
            &cfg,
        )
        .await
        .expect("scripted consult should succeed");

        let saved = sink.saved();
        assert_eq!(saved.len(), 1, "one save_artifact call, one artifact");
        assert_eq!(
            saved[0].digest,
            crate::cas::Digest::of_bytes(CORPUS.as_bytes()).to_hex(),
            "the artifact is addressed by the content the model actually wrote"
        );
        assert_eq!(saved[0].label, "the fuzz corpus");

        let delivered = crate::artifact::with_artifacts(out.answer, cfg.artifacts.as_deref());
        assert!(delivered.starts_with("ANSWER:"), "the answer stays first");
        assert!(
            delivered.contains(&format!("kaibo://cas/{}", saved[0].digest)),
            "the caller's footer must name the artifact's resource URI, got: {delivered}"
        );
        assert!(
            delivered.contains("the fuzz corpus"),
            "the footer carries the model's own label, got: {delivered}"
        );
    }

    /// Collect the text blocks of a user message (for asserting the finalize note).
    fn user_text(msg: &Message) -> String {
        match msg {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("expected a user message, got {msg:?}"),
        }
    }

    /// The usual cap shape: the transcript ends on the user's tool-results turn. The
    /// finalize note must ride *inside* that last user turn (not a new one — back-to-
    /// back user turns break some providers), and history must shrink by exactly it.
    #[test]
    fn finalize_folds_note_into_trailing_user_turn() {
        let history = vec![
            Message::user("Original question"),
            Message::assistant("calling a tool"),
            Message::user("tool results"),
        ];
        let (rest, prompt) = finalize_prompt(history, FINALIZE_NOTE);

        // The trailing user turn becomes the prompt and carries both its original
        // content and the appended note.
        let text = user_text(&prompt);
        assert!(
            text.contains("tool results"),
            "original content kept: {text}"
        );
        assert!(text.contains(FINALIZE_NOTE), "note appended: {text}");
        // History is everything before it — the trailing turn was consumed, not duplicated.
        assert_eq!(rest.len(), 2, "trailing user turn consumed into the prompt");
    }

    /// Defensive shape: if the transcript ends on an assistant turn, we must not
    /// mutate it — the note becomes a fresh trailing user turn (valid after an
    /// assistant message) and the assistant turn stays in history.
    #[test]
    fn finalize_adds_user_turn_when_transcript_ends_on_assistant() {
        let history = vec![Message::user("Q"), Message::assistant("partial thoughts")];
        let (rest, prompt) = finalize_prompt(history, FINALIZE_NOTE);

        assert!(
            user_text(&prompt).contains(FINALIZE_NOTE),
            "note is the new user turn"
        );
        assert_eq!(rest.len(), 2, "assistant turn kept in history");
        assert!(
            matches!(rest.last(), Some(Message::Assistant { .. })),
            "assistant turn preserved at the tail"
        );
    }

    // --- view_image user-turn rewrite (the openai VLM path) ------------------

    /// An assistant turn calling `view_image` by `id`.
    fn vi_call(id: &str) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(
                id,
                ViewImage::NAME,
                json!({ "path": "shot.png" }),
            )),
        }
    }

    /// A `view_image` tool result: the load note (text) *and* the image part — the
    /// two typed content blocks `ViewImage` returns.
    fn vi_result(id: &str) -> UserContent {
        UserContent::ToolResult(ToolResult {
            id: id.to_string(),
            call_id: None,
            content: OneOrMany::many([
                ToolResultContent::text("Loaded image shot.png (image/png, 1.0 KiB)."),
                ToolResultContent::image_base64("ZmFrZQ==", None, None),
            ])
            .unwrap(),
        })
    }

    /// True if any tool result anywhere still carries an image part.
    fn any_tool_result_image(h: &[Message]) -> bool {
        h.iter().any(|m| {
            matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr)
                    if tr.content.iter().any(|rc| matches!(rc, ToolResultContent::Image(_))))))
        })
    }

    /// Count user messages carrying an `Image` part (the rewrite's inserted turns).
    fn user_image_messages(h: &[Message]) -> usize {
        h.iter()
            .filter(|m| {
                matches!(m, Message::User { content }
                    if content.iter().any(|c| matches!(c, UserContent::Image(_))))
            })
            .count()
    }

    /// The core rewrite: a `view_image` image leaves the tool-result channel and
    /// reappears as its *own* tool-result-free user message, while the `tool_use`
    /// stays answered (now by text). The separate message is load-bearing — rig's
    /// openai converter drops non-tool parts from a mixed user turn.
    #[test]
    fn rewrite_moves_view_image_onto_a_separate_user_image_turn() {
        let history = vec![
            Message::user("look at shot.png"),
            vi_call("call-1"),
            Message::User {
                content: OneOrMany::one(vi_result("call-1")),
            },
        ];
        let out = rewrite_tool_image_history(history);

        assert!(
            !any_tool_result_image(&out),
            "no image may survive on the tool-result channel: {out:?}"
        );
        assert_eq!(
            user_image_messages(&out),
            1,
            "the image reappears as exactly one user Image message: {out:?}"
        );
        // The view_image tool_use is still answered (by a text-only result), so no
        // provider sees an orphaned tool_use.
        assert!(
            out.iter().any(|m| matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr)
                    if tr.id == "call-1"
                    && tr.content.iter().all(|rc| matches!(rc, ToolResultContent::Text(_))))))),
            "the view_image tool_use stays answered by a text result: {out:?}"
        );
        // The image turn lands *after* its (rewritten) tool-results message.
        let result_pos = out
            .iter()
            .position(|m| {
                matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
            })
            .expect("the tool-results message is present");
        let image_pos = out
            .iter()
            .position(|m| {
                matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::Image(_))))
            })
            .expect("the image message is present");
        assert!(
            result_pos < image_pos,
            "the image rides immediately after the tool result: {out:?}"
        );
    }

    /// Idempotent: a second pass (a later break re-walks the whole transcript) must
    /// not duplicate the image or otherwise change anything — it triggers only on a
    /// result that *still* holds an image, and the first pass already moved it.
    #[test]
    fn rewrite_is_idempotent() {
        let history = vec![
            Message::user("q"),
            vi_call("c1"),
            Message::User {
                content: OneOrMany::one(vi_result("c1")),
            },
        ];
        let once = rewrite_tool_image_history(history);
        let twice = rewrite_tool_image_history(once.clone());
        assert_eq!(once, twice, "a second rewrite pass is a no-op");
        assert_eq!(user_image_messages(&twice), 1, "no duplicate image turn");
    }

    /// Co-tool-call: one assistant turn called `view_image` *and* `run_kaish`, and one
    /// user turn answered both. The rewrite must move only the image and leave the
    /// `run_kaish` result verbatim — proof the rewrite never orphans the co-tool's
    /// `tool_use`. (The turn-boundary break that makes this transcript reachable is
    /// proven separately in the driven loop test.)
    #[test]
    fn rewrite_leaves_a_co_tool_call_result_intact() {
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::many([
                AssistantContent::tool_call("vi", ViewImage::NAME, json!({ "path": "shot.png" })),
                AssistantContent::tool_call("rk", "run_kaish", json!({ "script": "ls" })),
            ])
            .unwrap(),
        };
        let results = Message::User {
            content: OneOrMany::many([
                vi_result("vi"),
                UserContent::tool_result(
                    "rk",
                    OneOrMany::one(ToolResultContent::text("exit:0\nshot.png")),
                ),
            ])
            .unwrap(),
        };
        let out = rewrite_tool_image_history(vec![Message::user("q"), assistant, results]);

        assert!(!any_tool_result_image(&out), "view_image image moved out");
        assert_eq!(user_image_messages(&out), 1, "exactly the one image turn");
        assert!(
            out.iter().any(|m| matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr)
                    if tr.id == "rk"
                    && tr.content.iter().any(|rc| matches!(rc,
                        ToolResultContent::Text(t) if t.text.contains("shot.png"))))))),
            "the run_kaish tool_result is preserved verbatim: {out:?}"
        );
    }

    /// The generalization's whole point: an `explore` sweep's result carrying a
    /// routed image (the hybrid envelope `RunExplore::call` emits once it attached
    /// an image) gets EXACTLY the same user-turn-channel treatment as `view_image` —
    /// no per-tool `if` was added for it, because the rewrite keys on the result,
    /// not the tool's name.
    #[test]
    fn rewrite_moves_an_explore_result_image_onto_a_separate_user_image_turn() {
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(
                "explore-1",
                "explore",
                json!({ "question": "what does the diagram show?" }),
            )),
        };
        let result = Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: "explore-1".to_string(),
                call_id: None,
                content: OneOrMany::many([
                    ToolResultContent::text("attached: docs/arch.png (image/png, 4.0 KiB)"),
                    ToolResultContent::image_base64("ZmFrZQ==", None, None),
                ])
                .unwrap(),
            })),
        };
        let out = rewrite_tool_image_history(vec![Message::user("q"), assistant, result]);

        assert!(
            !any_tool_result_image(&out),
            "the explore result's image must leave the tool-result channel: {out:?}"
        );
        assert_eq!(
            user_image_messages(&out),
            1,
            "it reappears as exactly one user Image message: {out:?}"
        );
        assert!(
            out.iter().any(|m| matches!(m, Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr)
                    if tr.id == "explore-1"
                    && tr.content.iter().all(|rc| matches!(rc, ToolResultContent::Text(_))))))),
            "the explore tool_use stays answered by a text-only result: {out:?}"
        );
    }

    /// Two image-bearing results in ONE assistant turn (`view_image` + an `explore`
    /// sweep that routed a picture): the rewrite must move BOTH out — per-part
    /// filtering that stopped at the first image-bearing result would strand the
    /// second on a channel the transport rejects. (DeepSeek review gap, 2026-07-26.)
    #[test]
    fn rewrite_moves_both_images_when_two_tools_carry_them() {
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::many([
                AssistantContent::tool_call("vi", ViewImage::NAME, json!({ "path": "shot.png" })),
                AssistantContent::tool_call("ex", "explore", json!({ "question": "diagram?" })),
            ])
            .unwrap(),
        };
        let results = Message::User {
            content: OneOrMany::many([
                vi_result("vi"),
                UserContent::ToolResult(ToolResult {
                    id: "ex".to_string(),
                    call_id: None,
                    content: OneOrMany::many([
                        ToolResultContent::text("attached: docs/arch.png (image/png, 4.0 KiB)"),
                        ToolResultContent::image_base64("ZmFrZTI=", None, None),
                    ])
                    .unwrap(),
                }),
            ])
            .unwrap(),
        };
        let out = rewrite_tool_image_history(vec![Message::user("q"), assistant, results]);

        assert!(
            !any_tool_result_image(&out),
            "both images must leave the tool-result channel: {out:?}"
        );
        assert_eq!(
            user_image_messages(&out),
            2,
            "each image gets its own user Image turn: {out:?}"
        );
        for id in ["vi", "ex"] {
            assert!(
                out.iter().any(|m| matches!(m, Message::User { content }
                    if content.iter().any(|c| matches!(c, UserContent::ToolResult(tr)
                        if tr.id == id
                        && tr.content.iter().all(|rc| matches!(rc, ToolResultContent::Text(_))))))),
                "tool_use `{id}` stays answered by a text-only result: {out:?}"
            );
        }
    }

    /// The break keys on the DECLARED content parts, never on text: a *text* result
    /// that happens to quote image-looking JSON — an explorer cat-ing this very
    /// file, a JSON fixture, a grep hit — must not trip the break/rewrite machinery.
    /// On rig 0.41 the gate reads typed [`ToolOutput`] parts (no sniffing), so the
    /// false positive the Gemini review worried about (2026-07-26) is impossible by
    /// construction; this pins it against a regression to string matching.
    #[test]
    fn a_text_result_containing_the_image_literal_does_not_carry_an_image() {
        // A run_kaish-style plain string result quoting envelope-shaped JSON.
        let grep_hit = r#"exit:0
src/view_image.rs:177: json!({"response": note, "parts": [{"type":"image", "data": b64}]})"#;
        assert!(!tool_output_carries_image(&ToolOutput::text(grep_hit)));

        // JSON output shaped like the old hybrid envelope is still just JSON — only
        // a declared Image part counts.
        let envelope = json!({"response": "r", "parts": [{"type": "image", "data": "ZmFrZQ=="}]});
        assert!(!tool_output_carries_image(&ToolOutput::json(envelope)));

        // A genuinely declared image part — the block `view_image` and a routed
        // `explore` result emit — does carry.
        let with_image = ToolOutput::content(
            OneOrMany::many([
                ToolResultContent::text("note"),
                ToolResultContent::Image(Image {
                    data: DocumentSourceKind::Base64("ZmFrZQ==".into()),
                    media_type: ImageMediaType::from_mime_type("image/png"),
                    detail: None,
                    additional_params: None,
                }),
            ])
            .expect("two blocks is never empty"),
        );
        assert!(tool_output_carries_image(&with_image));
    }

    /// The outer turn budget is derived from the transcript (rig carries no
    /// `turns_used`): one model turn per assistant message, so a looping `view_image`
    /// can't refresh its budget every break.
    #[test]
    fn count_model_turns_counts_assistant_messages() {
        let history = vec![
            Message::user("q"),
            vi_call("a"),
            Message::User {
                content: OneOrMany::one(vi_result("a")),
            },
            Message::assistant("thinking"),
        ];
        assert_eq!(count_model_turns(&history), 2, "two assistant messages");
    }

    /// A *rewritten* transcript interleaves inserted user `Image` turns between the
    /// assistant turns; those must not inflate the count (they're `Message::User`, not
    /// assistant), or a looping `view_image` could refresh its budget after all.
    #[test]
    fn count_model_turns_ignores_inserted_user_image_turns() {
        let history = vec![
            Message::user("q"),
            vi_call("a"),
            Message::User {
                content: OneOrMany::one(vi_result("a")),
            },
            // The rewrite's inserted image turn — a user message, not a model turn.
            Message::User {
                content: OneOrMany::one(UserContent::image_base64("ZmFrZQ==", None, None)),
            },
            Message::assistant("now answering"),
        ];
        assert_eq!(
            count_model_turns(&history),
            2,
            "only the two assistant messages count; the inserted image turn does not"
        );
    }
}
