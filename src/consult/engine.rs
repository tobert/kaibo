//! The consult tool-loop engine and orchestration.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rig_core::agent::{HookAction, PromptHook};
use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    AssistantContent, Image, ImageMediaType, MimeType, ToolChoice, ToolResult, ToolResultContent,
    UserContent,
};
use rig_core::completion::{CompletionModel, Message, Prompt, PromptError, ToolDefinition};
use rig_core::providers::{anthropic, deepseek, gemini, openai, openrouter};
use rig_core::tool::{Tool, ToolDyn};
use rig_core::OneOrMany;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::attach::Attachment;
use crate::config::{Backend, Defaults, ModelRole, ModelSlot};
use crate::credentials::ProviderKind;
use crate::explorer::RunKaish;
use crate::progress::{PhaseEvent, ProgressSink};
use crate::sandbox::{KaishWorker, SandboxConfig};
use crate::session::{QaTurn, SessionStore};
use crate::tool_span::traced;
use crate::view_image::ViewImage;

use super::config::{ConsultConfig, ExploreConfig, PhaseContext};
#[cfg(test)]
use super::prompts::PromptOverrides;
use super::prompts::{consult_user_prompt, deliberation_prompt, resolve_phase_preamble, Phase};
use super::shaping::{ModelCaps, ModelShape};

// --- The Arm seam ------------------------------------------------------------

/// The toolset factory a phase loop rebuilds its tools from (once for the main
/// loop, again for the turn-cap finalize turn — see [`run_phase`]).
type ToolFactory<'a> = &'a (dyn Fn() -> Result<Vec<Box<dyn ToolDyn>>> + Send + Sync);

/// The object-safe seam one [`Arm`] runs its loops through: a (client, model)
/// pair erased behind a vtable. rig's provider clients are distinct concrete
/// types; monomorphizing every phase combination would be a kinds² macro product
/// (the decided plumbing fork, `docs/casts.md`) — the calls are network-bound,
/// so dynamic dispatch here is free. The one implementation, [`ClientArm`],
/// forwards to the generic [`run_phase`], which stays the offline-testable
/// primitive.
trait PhaseRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
    fn run_phase<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        initial_prompt: Message,
        max_turns: usize,
        params: Option<&'a Value>,
        progress: &'a dyn ProgressSink,
        make_tools: ToolFactory<'a>,
        break_on_view_image: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

/// The concrete (client, model) pair behind the [`PhaseRunner`] vtable.
struct ClientArm<C> {
    client: C,
    model: String,
}

impl<C> PhaseRunner for ClientArm<C>
where
    C: CompletionClient + Clone + Send + Sync + 'static,
    C::CompletionModel: 'static,
{
    fn run_phase<'a>(
        &'a self,
        preamble: &'a str,
        max_tokens: u64,
        initial_prompt: Message,
        max_turns: usize,
        params: Option<&'a Value>,
        progress: &'a dyn ProgressSink,
        make_tools: ToolFactory<'a>,
        break_on_view_image: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(run_phase(
            &self.client,
            &self.model,
            preamble,
            max_tokens,
            initial_prompt,
            max_turns,
            params,
            progress,
            make_tools,
            break_on_view_image,
        ))
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
        Self {
            runner: Arc::new(ClientArm {
                client,
                model: model.clone(),
            }),
            model,
            max_tokens,
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
        // Re-assert the budget rule at the single live construction point: config
        // load validates every *configured* slot, but per-call overrides build
        // bare slots that never saw it — an inverted pair must be the same
        // keyworded boundary error here, not a provider 400 mid-call. Checked
        // before key resolution so it fires with no key configured.
        if matches!(backend.kind, ProviderKind::Anthropic | ProviderKind::Gemini)
            && t.thinking_budget >= t.max_tokens
        {
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
        let params = ModelShape::resolve(backend.kind, &slot.id, t.thinking_style).to_params(
            t.thinking_budget,
            Some(t.temperature),
            Some(t.top_p),
            &t.effort,
        );
        // OpenRouter drops rig's native `max_tokens` (see `inject_output_budget`), so
        // the budget must ride `additional_params` as `max_completion_tokens`. A no-op
        // for every other kind, whose `max_tokens` rig sends itself.
        let params = super::shaping::inject_output_budget(backend.kind, params, t.max_tokens);
        let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);

        // One HTTP backend carrying the per-request deadline, built by the shared
        // `crate::tls::https_client` (ring installed, `rustls-no-provider`, no OpenSSL/C —
        // the one client-build site). It bounds the otherwise-brakeless non-streaming call
        // (the 2026-06-06 wedge; see the helper's doc). Injected via rig's `.http_client(..)`.
        let http = crate::tls::https_client(backend.request_timeout)?;

        match backend.kind {
            ProviderKind::Anthropic => {
                let key = backend.resolve_key()?;
                let client = anthropic::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("anthropic client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            ProviderKind::DeepSeek => {
                let key = backend.resolve_key()?;
                let client = deepseek::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("deepseek client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            ProviderKind::Gemini => {
                let key = backend.resolve_key()?;
                let client = gemini::Client::builder()
                    .api_key(&key)
                    .http_client(http)
                    .build()
                    .map_err(|e| anyhow!("gemini client init: {e}"))?;
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            ProviderKind::OpenRouter => {
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
                Ok(Self::new(client, &slot.id, t.max_tokens, params, caps))
            }
            ProviderKind::Openai => {
                // Any OpenAI-compatible endpoint, addressed by the backend's base
                // URL. The key is optional for a keyless backend: `resolve_key`
                // returns the configured key or a placeholder the server ignores.
                let base_url = backend.resolved_base_url();
                let key = backend.resolve_key()?;
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

    /// Does a `view_image` on this arm need the user-turn rewrite? True exactly when
    /// the model can *see* but its transport can't carry the image in a tool result
    /// (an OpenAI VLM) — the predicate [`run_phase`]'s break-rewrite-resume gate reads.
    /// A blind arm never sees `view_image`, so this is false there regardless.
    fn rewrites_view_image(&self) -> bool {
        self.caps.vision && !self.caps.tool_result_images
    }

    /// Run one bounded tool loop on this arm: its client, model, params, and
    /// `max_tokens`, with the caller's preamble/prompt/turn-cap/toolset.
    pub(crate) async fn run(
        &self,
        preamble: &str,
        initial_prompt: Message,
        max_turns: usize,
        progress: &dyn ProgressSink,
        make_tools: ToolFactory<'_>,
    ) -> Result<String> {
        self.runner
            .run_phase(
                preamble,
                self.max_tokens,
                initial_prompt,
                max_turns,
                self.params.as_ref(),
                progress,
                make_tools,
                self.rewrites_view_image(),
            )
            .await
    }
}

/// The result of a consult: the final answer plus the explorer's report (kept so
/// callers can inspect/debug the hand-off, and for future session storage).
#[derive(Debug, Clone)]
pub struct ConsultOutput {
    pub answer: String,
    pub report: String,
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
COMPLETE final response now, with its concrete `file:line` citations. Do not call \
any tool. Do not ask to continue. Write the full answer (or curated report) from \
what you already have — right now.";

/// Build the forced final turn from the transcript rig hands back at the turn cap.
///
/// Returns `(history, prompt)` for one more constrained completion: the prompt is
/// the conversation's last message with [`FINALIZE_NOTE`] appended (so the model
/// reads the "answer now" instruction last), and `history` is everything before it.
///
/// At the cap the transcript's last message is almost always the user's
/// tool-results turn (the loop broke just as the model was about to call yet
/// another tool), so the note rides along inside that same user message — we never
/// emit two user turns back to back, which some providers reject. If the transcript
/// somehow ends on an assistant turn, the note becomes a fresh trailing user turn
/// instead (valid after an assistant message). Pure and offline-testable.
fn finalize_prompt(mut chat_history: Vec<Message>) -> (Vec<Message>, Message) {
    match chat_history.pop() {
        Some(Message::User { mut content }) => {
            content.push(UserContent::text(FINALIZE_NOTE));
            (chat_history, Message::User { content })
        }
        Some(other) => {
            chat_history.push(other);
            (chat_history, Message::user(FINALIZE_NOTE))
        }
        None => (Vec::new(), Message::user(FINALIZE_NOTE)),
    }
}

// --- view_image on the user-turn channel (the openai VLM path) ----------------

/// The cancellation reason [`ViewImageBreakHook`] terminates with, so [`run_phase`]
/// can tell *its* deliberate break from any other `PromptCancelled` rig might raise
/// (a lost prompt, an empty tool batch). An internal sentinel; never shown to a model.
const VIEW_IMAGE_BREAK: &str = "kaibo:view_image_break";

/// The text a rewritten `view_image` tool result carries when its own note is somehow
/// absent — enough to satisfy the `tool_use → tool_result` pairing every provider
/// requires. The image itself rides the separate user turn the rewrite inserts.
const VIEW_IMAGE_ACK: &str = "Loaded the requested image; it is shown in the next message.";

/// Breaks the managed tool loop at the turn boundary after a `view_image` ran, so
/// [`run_phase`] can move the image onto the **user-turn** channel for a transport
/// that can't carry it in a tool result (an OpenAI VLM).
///
/// **Why flag now, terminate next — not mid-turn.** A single assistant turn can call
/// `view_image` *and* `run_kaish` together; terminating the instant `view_image`
/// returns would drop the other tool's result and orphan its `tool_use`. And rig's
/// `on_tool_result` Terminate hands back a transcript snapshotted *before* the turn's
/// results are folded into `new_messages` (`prompt_request/mod.rs` ~:928), so it
/// wouldn't even carry the image we came for. So we only *set a flag* on
/// `on_tool_result`, and terminate on the **next** `on_completion_call` — the point
/// where rig has written every tool result of the triggering turn into `new_messages`
/// and `Terminate` returns the complete transcript (`:670`). Disabled
/// (`enabled == false`) every callback is a no-op, so installing it on a transport
/// that carries tool-result images (Anthropic/Gemini) is byte-for-byte the old path.
#[derive(Clone)]
struct ViewImageBreakHook {
    enabled: bool,
    /// Set once a `view_image` tool result lands this turn; read at the next
    /// completion call. Interior mutability because `PromptHook`'s callbacks are `&self`
    /// and rig runs a turn's tools concurrently.
    saw_view_image: Arc<AtomicBool>,
}

impl ViewImageBreakHook {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            saw_view_image: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<M: CompletionModel> PromptHook<M> for ViewImageBreakHook {
    async fn on_tool_result(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        _args: &str,
        _result: &str,
    ) -> HookAction {
        if self.enabled && tool_name == ViewImage::NAME {
            self.saw_view_image.store(true, Ordering::SeqCst);
        }
        HookAction::cont()
    }

    async fn on_completion_call(&self, _prompt: &Message, _history: &[Message]) -> HookAction {
        if self.enabled && self.saw_view_image.load(Ordering::SeqCst) {
            HookAction::terminate(VIEW_IMAGE_BREAK)
        } else {
            HookAction::cont()
        }
    }
}

/// Rewrite a transcript so every `view_image` image rides the **user-turn** channel
/// instead of the tool-result channel. For each `view_image` tool result that still
/// carries an image: keep its text as a short ack (so the `tool_use → tool_result`
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
fn rewrite_view_image_history(history: Vec<Message>) -> Vec<Message> {
    // The tool_use ids naming a view_image call live on the *assistant* `ToolCall`,
    // not on the user `ToolResult` — collect them first, then match results against them.
    let view_image_ids: HashSet<String> = history
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .flat_map(|content| content.iter())
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) if tc.function.name == ViewImage::NAME => {
                Some(tc.id.clone())
            }
            _ => None,
        })
        .collect();

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
        // Images pulled out of this turn's view_image results, re-emitted as their own
        // user messages immediately after (one per image), preserving order.
        let mut extracted: Vec<Image> = Vec::new();

        for part in content {
            match part {
                UserContent::ToolResult(tr) if view_image_ids.contains(&tr.id) => {
                    let ToolResult {
                        id,
                        call_id,
                        content,
                    } = tr;
                    // Split the result into its text (the load note → ack) and its
                    // image (→ a user turn). A result already acked has no image, so
                    // this is a no-op for it — the idempotency that makes re-running safe.
                    let mut texts: Vec<ToolResultContent> = Vec::new();
                    for rc in content {
                        match rc {
                            ToolResultContent::Image(img) => extracted.push(img),
                            text => texts.push(text),
                        }
                    }
                    let content = OneOrMany::many(texts).unwrap_or_else(|_| {
                        OneOrMany::one(ToolResultContent::text(VIEW_IMAGE_ACK))
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
/// `Box<dyn ToolDyn>` can't be cloned, so we rebuild rather than share. Each call
/// spawns its own `KaishWorker`(s); the caller owns their lifetime.
///
/// **Turn-cap recovery.** When the model uses every turn without concluding, rig
/// 0.34 returns `MaxTurnsError` carrying the *full transcript* (not the opaque
/// failure the old code mapped to an error). Rather than discard all that work, we
/// run one final constrained turn via [`finalize_after_max_turns`]: the tools stay
/// declared so the accumulated tool_use/tool_result history stays valid, but
/// `ToolChoice::None` forbids new calls — the model must answer from what it has.
///
/// **view_image on the user-turn channel.** When `break_on_view_image` is set (a
/// vision model whose transport can't carry an image in a tool result — an OpenAI
/// VLM), a [`ViewImageBreakHook`] terminates the loop at the turn boundary after a
/// `view_image` call. rig hands back the full transcript via `PromptCancelled`; we
/// rewrite each `view_image` result onto a separate user `Image` turn
/// ([`rewrite_view_image_history`]) and re-enter the loop with the remaining turn
/// budget. The model now sees the image in user content, the one channel every
/// provider accepts. When unset the hook is inert and this is the old single call.
#[allow(clippy::too_many_arguments)]
// each arg is a distinct, named loop input
// A named parent for rig's GenAI spans: rig's `invoke_agent` checks the current
// span and nests under this one, so a phase's whole model loop (every `chat` turn,
// every `tool` call) hangs off one `run_phase` span carrying the model. Inert
// unless an exporter is attached (telemetry off → no subscriber records it).
#[tracing::instrument(name = "run_phase", skip_all, fields(model = %model, max_turns = max_turns))]
pub(crate) async fn run_phase<C, F>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    initial_prompt: Message,
    max_turns: usize,
    thinking: Option<&Value>,
    progress: &dyn ProgressSink,
    make_tools: F,
    break_on_view_image: bool,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
    F: Fn() -> Result<Vec<Box<dyn ToolDyn>>>,
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
                client,
                model,
                preamble,
                max_tokens,
                thinking,
                make_tools()?,
                full,
                max_turns,
            )
            .await;
        }

        let mut builder = client
            .agent(model)
            .preamble(preamble)
            .max_tokens(max_tokens);
        // Thinking on (both phases) where the provider takes a request-time toggle.
        if let Some(params) = thinking {
            builder = builder.additional_params(params.clone());
        }
        let agent = builder.tools(make_tools()?).build();

        // A fresh hook per loop iteration is load-bearing: its `saw_view_image` flag
        // must be scoped to *this* turn. Hoisting it out of the loop (or reusing the
        // agent across resumes) would carry a stale flag — breaking on the first
        // completion call of a resume that ran no view_image. Keep it built here.
        let result = agent
            .prompt(prompt.clone())
            .with_history(history.clone())
            .with_hook(ViewImageBreakHook::new(break_on_view_image))
            .max_turns(remaining)
            .await;

        match result {
            Ok(answer) => return Ok(answer),
            Err(PromptError::MaxTurnsError { chat_history, .. }) => {
                // The loop hit its cap and is about to write a forced final answer —
                // tell the caller, so a watching client sees "wrapping up" not silence.
                progress.emit(PhaseEvent::TurnCapReached);
                return finalize_after_max_turns(
                    client,
                    model,
                    preamble,
                    max_tokens,
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
            }) if reason == VIEW_IMAGE_BREAK => {
                let (rest, next) = split_for_resume(rewrite_view_image_history(chat_history));
                history = rest;
                prompt = next;
            }
            Err(e) => return Err(anyhow!("model loop failed: {e}")),
        }
    }
}

/// The forced final turn after a phase hit its turn cap: replay the partial
/// transcript and make the model write its answer now, with tools declared (so the
/// history validates) but [`ToolChoice::None`] forbidding any further call. See
/// [`run_phase`]'s recovery note and [`finalize_prompt`].
#[allow(clippy::too_many_arguments)] // mirrors run_phase's loop inputs
async fn finalize_after_max_turns<C>(
    client: &C,
    model: &str,
    preamble: &str,
    max_tokens: u64,
    thinking: Option<&Value>,
    tools: Vec<Box<dyn ToolDyn>>,
    chat_history: Vec<Message>,
    max_turns: usize,
) -> Result<String>
where
    C: CompletionClient,
    C::CompletionModel: 'static,
{
    let (history, prompt) = finalize_prompt(chat_history);
    let mut builder = client
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .tool_choice(ToolChoice::None);
    if let Some(params) = thinking {
        builder = builder.additional_params(params.clone());
    }
    let agent = builder.tools(tools).build();
    // max_turns(1): one constrained completion. With tools forbidden the model can't
    // loop, so a single round is enough — and if a provider ignores ToolChoice::None
    // and still calls a tool, we surface that rather than recurse.
    agent
        .prompt(prompt)
        .with_history(history)
        .max_turns(1)
        .await
        .map_err(|e| {
            anyhow!(
                "model used all {max_turns} turns, and the forced final-answer turn \
                 also failed to conclude: {e}"
            )
        })
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
pub(crate) async fn run_explore_phase(
    arm: &Arm,
    preamble: &str,
    question: &str,
    root: PathBuf,
    sandbox: &SandboxConfig,
    max_turns: usize,
    progress: &Arc<dyn ProgressSink>,
) -> Result<String> {
    arm.run(
        preamble,
        Message::user(question.to_string()),
        max_turns,
        progress.as_ref(),
        &|| -> Result<Vec<Box<dyn ToolDyn>>> {
            Ok(vec![traced(RunKaish::with_progress(
                KaishWorker::spawn_with(&root, sandbox.clone())?,
                progress.clone(),
            ))])
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
    /// Liveness for the sweep: brackets each delegation with start/finish, and is
    /// handed to the nested kernel's `run_kaish` so the sub-agent's own reads show
    /// through too (a delegated sweep is where a long consult spends its silence).
    progress: Arc<dyn ProgressSink>,
    /// The sweep's fully-resolved system prompt, computed once by `consult_tools`:
    /// the explorer override-or-default with house rules already appended. So the
    /// nested explorer carries the explorer's `[prompts]`/`[context]` framing,
    /// built once instead of per sweep.
    preamble: Arc<str>,
}

impl RunExplore {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        arm: Arm,
        max_turns: usize,
        root: impl Into<PathBuf>,
        sandbox: SandboxConfig,
        reports: Arc<Mutex<Vec<String>>>,
        progress: Arc<dyn ProgressSink>,
        preamble: Arc<str>,
    ) -> Self {
        Self {
            arm,
            max_turns,
            root: root.into(),
            sandbox,
            reports,
            progress,
            preamble,
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
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Delegate a broad sweep to a fast investigator that rips \
                through the repo on a read-only kaish shell and reports back with \
                concrete `file:line` citations. Give it a focused question; use it \
                to cover breadth, and read the code yourself with `run_kaish`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "the question or sub-question to investigate"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Bracket the delegation: the start carries the sub-question, the finish fires
        // on both success and failure (the `?` below short-circuits, so emit it before
        // unwrapping the result).
        self.progress.emit(PhaseEvent::SweepStarted {
            question: args.question.clone(),
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
        )
        .await;
        self.progress.emit(PhaseEvent::SweepFinished);
        let report = result.map_err(|e| RunExploreError(format!("{e:#}")))?;
        // Lock poisoning means another delegation panicked — surface it, don't mask.
        self.reports
            .lock()
            .expect("explore report sink poisoned")
            .push(report.clone());
        Ok(report)
    }
}

/// The `oneshot` seam: one direct completion on the resolved arm — no tools, no
/// shell, no exploration. The thin counterpart to `consult` (prompt in, answer out)
/// for when the caller already owns the context and just wants the model's take.
/// Built on the one loop primitive with an empty toolset and a single turn, so it is
/// exactly one upstream request. Neither orientation nor operator house rules are
/// spliced — both are project guidance, and oneshot reads no project.
///
/// Safe-by-accident note: a vision synth arm carries `break_on_view_image = true`
/// (via `Arm::rewrites_view_image`), but the empty toolset has no `view_image` tool,
/// so the break hook can never fire and the single turn always completes cleanly. If
/// you ever give oneshot a tool, revisit this — a live break would consume the turn
/// and land in the turn-cap finalize path.
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
) -> Result<String> {
    let user_prompt = crate::attach::with_text_context(attachments, prompt);
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
    // Assemble the single user turn here — multimodal awareness lives in oneshot, not the
    // shared loop. No images → a bare `Message::user` (byte-for-byte the old call). With
    // images, the text rides as the first part (skipped when empty — image-only with an
    // empty prompt shouldn't emit a pointless `{type:text,text:""}` block), then the images.
    let initial_prompt = if image_parts.is_empty() {
        Message::user(user_prompt)
    } else {
        let mut parts = Vec::with_capacity(image_parts.len() + 1);
        if !user_prompt.is_empty() {
            parts.push(UserContent::text(user_prompt));
        }
        parts.extend(image_parts);
        Message::User {
            content: OneOrMany::many(parts).expect("image_parts is non-empty on this branch"),
        }
    };
    with_call_deadline(
        cfg.call_deadline,
        "oneshot",
        arm.run(
            &resolve_phase_preamble(
                Phase::Oneshot,
                &cfg.prompts,
                cfg.orientation.as_deref(),
                cfg.house_rules.as_deref(),
            ),
            initial_prompt,
            1,
            cfg.progress.as_ref(),
            &|| Ok(Vec::new()),
        ),
    )
    .await
}

/// Build the recomposed `consult` toolset: `{run_kaish, explore′}`. Factored out so
/// the wiring (both tools present, explore′ pointed at the explorer arm) is
/// unit-testable without a live model. `reports` collects each `explore′` sweep.
fn consult_tools(
    explorer: &Arm,
    root: &Path,
    cfg: &ConsultConfig,
    reports: Arc<Mutex<Vec<String>>>,
    synth_vision: bool,
) -> Result<Vec<Box<dyn ToolDyn>>> {
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
    let explore = RunExplore::new(
        explorer.clone(),
        cfg.explore.explorer_max_turns,
        root,
        cfg.explore.sandbox.clone(),
        reports,
        cfg.explore.phase.progress.clone(),
        explorer_preamble,
    );
    let mut tools: Vec<Box<dyn ToolDyn>> = vec![
        traced(RunKaish::with_progress(
            worker.clone(),
            cfg.explore.phase.progress.clone(),
        )),
        traced(explore),
    ];
    // The driver loop runs on the *synth* arm, so view_image rides the synth's
    // vision cap (the delegated explore′ sub-agent gets its own view_image keyed to
    // the explorer arm's caps, inside `explore`). Shares the driver's kernel.
    if synth_vision {
        tools.push(traced(ViewImage::new(worker, root.to_path_buf())));
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
) -> Result<String> {
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
pub async fn deliberate_direct(
    question: &str,
    dossier: &str,
    synth: &Arm,
    system: &str,
    deadline: Duration,
    progress: &Arc<dyn ProgressSink>,
) -> Result<String> {
    with_call_deadline(
        deadline,
        "deliberate",
        synth.run(
            system,
            Message::user(deliberation_prompt(question, dossier)),
            1,
            progress.as_ref(),
            &|| Ok(Vec::new()),
        ),
    )
    .await
}

/// Run a `consult` over two resolved arms.
///
/// One loop, two tools — no rigid explorer→synth hand-off. The capable model
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

    let answer = with_call_deadline(
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
            // turn); every build shares the one `reports` sink so all explore′
            // sweeps aggregate.
            &|| consult_tools(explorer, root, cfg, reports.clone(), synth.caps.vision),
        ),
    )
    .await
    .context("consult loop")?;

    let report = reports
        .lock()
        .expect("explore report sink poisoned")
        .join("\n\n---\n\n");
    Ok(ConsultOutput { answer, report })
}

/// A consult turn's session binding: the store and the session id. `None` is a
/// stateless one-shot — no prior turns replayed, nothing recorded. `Some` makes it
/// multi-turn: replay this session's history into the prompt, record the answer.
pub type Session<'a> = (&'a SessionStore, &'a str);

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
    let history = match session {
        Some((store, id)) => store.history(id),
        None => Vec::new(),
    };
    let user_prompt = consult_user_prompt(question, context, &history, &cfg.attachments);

    let out = consult_with(&user_prompt, root, explorer, synth, cfg).await?;

    if let Some((store, id)) = session {
        store.record(id, QaTurn::new(question, out.answer.clone()));
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

    use crate::session::SessionStore;
    use crate::test_support::{
        has_tool, is_finalize_turn, provider_error, text_response, tool_call_response,
        transcript_text, RecordingSink, ScriptedClient,
    };
    use std::fs;
    use std::num::NonZeroUsize;
    use tempfile::tempdir;

    fn store() -> SessionStore {
        SessionStore::new(NonZeroUsize::new(4).unwrap())
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
            pre.contains("You answer a question about a codebase"),
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
            pre2.contains("You answer a question about a codebase"),
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
            explorer_pre.contains("code explorer"),
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
            !pre.contains("You answer a question about a codebase"),
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
        // role), the capable model saw the *consult* preamble (driver role).
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
                .contains("code explorer"),
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

        let report = explore_with(
            "Where is target_marker defined?",
            dir.path().to_path_buf(),
            &arm(&client, EXPLORER),
            &cfg,
            &[],
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
                .contains("code explorer"),
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
        let out = deliberate_direct(
            "Is the retry path safe?",
            "src/retry.rs:12 DOSSIER_MARKER fn retry()",
            &arm(&client, SYNTH),
            "You are a capable model answering a hard question offline.",
            cfg.call_deadline,
            &cfg.progress,
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
                &arm(&client, SYNTH),
                "offline synth preamble",
                Duration::from_millis(50),
                &(Arc::new(crate::progress::NullSink) as Arc<dyn ProgressSink>),
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

    /// The per-phase payoff: when a cast's synth and explorer straddle the Gemini
    /// 3-line capability boundary, each request must carry the thinking shape fit to
    /// *its own* model — the driver `thinkingLevel`, the sweep `thinkingBudget`. A
    /// regression that resolved thinking once and shared it — the old
    /// profile-level shape — would put one model's params on the other's request.
    #[tokio::test]
    async fn each_phase_gets_thinking_fit_to_its_own_model() {
        const SYNTH: &str = "gemini-3-pro-preview"; // 3-line → thinkingLevel
        const EXPLORER: &str = "gemini-2.5-flash"; // 2.5 → thinkingBudget

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
                thinking_params(ProviderKind::Gemini, SYNTH, 4096),
            ),
            &cfg,
        )
        .await
        .unwrap();

        let tc = |r: &crate::test_support::RecordedRequest| {
            r.additional_params.as_ref().unwrap()["generationConfig"]["thinkingConfig"].clone()
        };
        for r in client.requests_for(SYNTH) {
            let cfg = tc(&r);
            assert_eq!(cfg["thinkingLevel"], "high", "3-line driver wants a level");
            assert!(
                cfg.get("thinkingBudget").is_none(),
                "level and budget are exclusive"
            );
        }
        for r in client.requests_for(EXPLORER) {
            let cfg = tc(&r);
            assert_eq!(cfg["thinkingBudget"], 4096, "2.5 explorer wants a budget");
            assert!(
                cfg.get("thinkingLevel").is_none(),
                "level and budget are exclusive"
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
        };
        let slot = ModelSlot::bare("anthropic", "claude-haiku-4-5");
        let err = Arm::from_slot(&backend, &slot, ModelRole::Explorer, &defaults)
            .expect_err("an inverted budget must be caught at arm construction");
        let msg = format!("{err:#}");
        assert!(msg.contains("thinking_budget"), "got: {msg}");
        assert!(msg.contains("max_tokens"), "got: {msg}");
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
            sessions.history(sid),
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
            sessions.history(sid).len(),
            2,
            "both turns accumulate in the thread"
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
            sessions.history(sid).is_empty(),
            "a failed turn must leave the thread untouched, got: {:?}",
            sessions.history(sid)
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
            sessions.session_count(),
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

        let tools = consult_tools(
            &arm(&client, "explorer-model"),
            dir.path(),
            &cfg,
            reports,
            false, // synth is not vision-capable
        )
        .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
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

        let tools = consult_tools(
            &arm(&client, "explorer-model"),
            dir.path(),
            &cfg,
            reports,
            true, // synth IS vision-capable
        )
        .expect("building the consult toolset should succeed");

        let names: Vec<String> = tools.iter().map(|t| t.name()).collect();
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

        let blind = consult_tools(&explorer, dir.path(), &cfg, reports.clone(), false)
            .expect("blind toolset builds");
        let blind_names: Vec<String> = blind.iter().map(|t| t.name()).collect();
        assert!(blind_names.iter().any(|n| n == "run_kaish"));
        assert!(blind_names.iter().any(|n| n == "explore"));
        assert!(
            !blind_names.iter().any(|n| n == "view_image"),
            "no view_image without vision, got {blind_names:?}"
        );

        let seeing = consult_tools(&explorer, dir.path(), &cfg, reports, true)
            .expect("vision toolset builds");
        let seeing_names: Vec<String> = seeing.iter().map(|t| t.name()).collect();
        assert!(
            seeing_names.iter().any(|n| n == "view_image"),
            "view_image present with vision, got {seeing_names:?}"
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
        let (rest, prompt) = finalize_prompt(history);

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
        let (rest, prompt) = finalize_prompt(history);

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
    /// hybrid shape rig's `from_tool_output` produces.
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
        let out = rewrite_view_image_history(history);

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
        let once = rewrite_view_image_history(history);
        let twice = rewrite_view_image_history(once.clone());
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
        let out = rewrite_view_image_history(vec![Message::user("q"), assistant, results]);

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
