//! `consult` and the seams it's composed from, across providers.
//!
//! One primitive — [`run_phase`]: a model + preamble + an injected toolset, run as
//! a bounded tool loop. Every tool on the surface is that loop wearing different
//! clothes:
//!
//! - [`explore`] — a cheap model · `{run_kaish}` → a curated report.
//! - [`synthesize`] — a capable model · `{run_kaish}` · optional context → an answer.
//! - [`consult`] — a capable model · `{run_kaish, explore′}` → a cited answer. No
//!   rigid explorer→synth hand-off: the capable model decides when to delegate a
//!   broad sweep to the cheap [`RunExplore`] sub-agent vs. read a span directly.
//!
//! Each phase arrives as a resolved [`Arm`]: its own client (type-erased — the
//! decided plumbing fork, `docs/casts.md`), model, request params, and caps. The
//! server resolves a cast's slots into arms ([`Arm::from_slot`]); a cast whose
//! explorer and synth live on different backends — different wire protocols,
//! even — runs each phase on its own client through the same loop primitive.
//! Each tool gets its own fresh [`KaishWorker`] (a kernel rooted at the
//! project), and so does every `explore′` delegation.

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
    AssistantContent, Image, ToolChoice, ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::{CompletionModel, Message, Prompt, PromptError, ToolDefinition};
use rig_core::providers::{anthropic, deepseek, gemini, openai};
use rig_core::tool::{Tool, ToolDyn};
use rig_core::OneOrMany;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{Backend, Defaults, ModelRole, ModelSlot};
use crate::credentials::ProviderKind;
use crate::explorer::RunKaish;
use crate::kaish_syntax::kaish_syntax_core;
use crate::progress::{NullSink, PhaseEvent, ProgressSink};
use crate::sandbox::{KaishWorker, SandboxConfig};
use crate::session::{QaTurn, SessionStore};
use crate::view_image::ViewImage;

/// Splice the operator's house rules (if any) onto a phase preamble. The base
/// preamble functions stay pure (and their tests byte-for-byte stable); this is
/// the one seam that folds in the assembled `[context]` block. Every phase that
/// drives a model uses it — the `consult` driver, the standalone `explore` and
/// `synthesize`, *and* the nested `explore′` sweep — so the explorer orients on
/// the same guidance the driver does (it helps *search*, not just the answer).
/// `None` returns the base unchanged: a server with no `[context]` files runs
/// exactly the historical preamble.
///
/// Framed as standing background, not the question, and positively (per the
/// `positive-prompt-framing` discipline): tell the model what the block *is* and
/// how to use it — conventions to honor while investigating — rather than fencing
/// it off. It sits *after* the base so the tool's own role framing leads.
fn with_house_rules(base: String, house_rules: Option<&str>) -> String {
    match house_rules {
        None => base,
        Some(rules) => format!(
            "{base}\n\n\
             --- Operator house rules for this codebase ---\n\
             The agent you're helping configured the guidance below — project \
             conventions and working preferences for this repository. Treat it as \
             trusted standing context: honor it as you investigate and when you write \
             your answer. It's background about how this codebase works, not the \
             question you're answering.\n\n{rules}"
        ),
    }
}

/// Operator preamble (system-prompt) overrides per phase, from the `[prompts]`
/// config table. `None` for a phase means "use the built-in" — so an empty table
/// is byte-for-byte the historical preambles. **Full replace** by decision: an
/// override *is* the role framing, verbatim; the kaish operating contract is not
/// re-appended here because it independently rides the `run_kaish` tool
/// description (`run_kaish_tool_description`), so the model keeps the shell
/// contract even when an operator rewrites the prose. Empty/whitespace values are
/// refused at config load (`config.rs::merge_prompts`) — a blank system prompt is
/// never the intent. House rules still append on top (see [`phase_preamble`]):
/// `[prompts]` replaces the *role* framing, `[context]` adds *project* guidance —
/// orthogonal axes.
#[derive(Debug, Clone, Default)]
pub struct PromptOverrides {
    /// Replaces [`report_preamble`] — the explorer, both standalone and the
    /// nested `explore′` sweep.
    pub explorer: Option<String>,
    /// Replaces [`synthesize_preamble`] — the standalone `synthesize`.
    pub synthesize: Option<String>,
    /// Replaces [`consult_preamble`] — the `consult` driver.
    pub consult: Option<String>,
}

/// Resolve one phase's full system prompt: the operator override if set, else the
/// built-in `default`, then house rules appended. The single composition point for
/// every model-driven phase, so override + `[context]` layering is identical
/// everywhere (and the call sites read as one line).
fn phase_preamble(
    override_: Option<&str>,
    default: fn() -> String,
    house_rules: Option<&str>,
) -> String {
    let base = override_.map(str::to_string).unwrap_or_else(default);
    with_house_rules(base, house_rules)
}

/// Explorer preamble: gather and organize evidence, don't conclude. Composes the
/// shared [`kaish_syntax_core`] so the shell idioms and exit-code contract are
/// stated in exactly one place.
pub fn report_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You are a code explorer. You build a complete, accurate picture of the code \
         a question touches and hand it to a synthesizer who writes the final \
         answer — so your work is to gather grounded evidence and cite it exactly. \
         {core}\n\n\
         HOW TO READ. Read for the whole picture in as few looks as possible. Locate \
         with ripgrep and take the surrounding context in the same call — \
         `rg -n -B4 -A8 PATTERN` returns each match with the lines around it, ready \
         to understand. When a file is central, read it WHOLE with `cat -n FILE`: \
         most files are short, and one full read hands you its imports, its context, \
         and exact line numbers together. Save narrow spans for a genuinely large \
         file — over a thousand lines, which `wc -l FILE` confirms: \
         `cat -n FILE | sed -n 'A,Bp'`.\n\n\
         HOW TO INVESTIGATE. Aim for the complete set of relevant locations. Follow \
         each key symbol to where it is defined and where it is used; chase anything \
         that puzzles you until it is clear — a confusing spot usually hides the \
         thing you need. One thorough pass beats many shallow ones.\n\n\
         WHAT TO PRODUCE. A curated report for the synthesizer, in these sections:\n\
         - SummaryOfFindings: what you concluded, in a few sentences.\n\
         - RelevantLocations: for each location that matters — the concrete \
         `file:line`, the key symbols there (functions, types, fields), a short \
         verbatim snippet, and what it means for the question.\n\
         - ExplorationTrace: the path you took, when it helps the synthesizer trust \
         the result.\n\
         Keep it tight and evidence-first. The synthesizer trusts your citations and \
         builds on them, so ground every claim in an exact `file:line` — that \
         exactness is the whole value of your report."
    )
}

/// Token budget for model "thinking"/reasoning, for the providers that expose a
/// request-time toggle. Sized well under [`ConsultConfig`]'s `max_tokens` so the
/// reasoning never starves the actual answer (a thinking model that spends its
/// whole budget reasoning returns empty content — we saw exactly that on Gemma).
/// Anthropic additionally *requires* `max_tokens > budget_tokens`.
pub const THINKING_BUDGET: u64 = 8192;

/// Does this Gemini id belong to the *pure* 3-line (e.g. `gemini-3-pro-preview`),
/// which takes `thinkingLevel` rather than the 2.5-era `thinkingBudget`?
///
/// The boundary is **empirical, not nominal**: `gemini-3.5-flash` *accepted*
/// `thinkingBudget` in the 2026-06-06 live test, so switching it to `thinkingLevel`
/// would be a silent regression of a working default. We only flip the ids the
/// official API + gemini-cli confirm want a level — `gemini-3-…` — and leave the
/// `3.5` minor line (and 2.x) on budget. Any new id past these wants a live probe,
/// not a guess. See `docs/issues.md` "Per-model request shaping".
fn is_gemini3_level(model: &str) -> bool {
    model == "gemini-3" || model.starts_with("gemini-3-")
}

/// The per-role thinking-depth lever for the models that expose one as a request
/// param (Anthropic adaptive's `output_config.effort`, DeepSeek's `reasoning_effort`).
/// A passthrough string the provider validates — like a model id — so a new level
/// lands without a code change. Default for both roles unless a slot or the
/// per-role `[defaults]` tunes it.
pub const DEFAULT_EFFORT: &str = "high";

/// Which Anthropic models want **adaptive** thinking (`{type:"adaptive"}` plus an
/// `output_config.effort`) instead of the legacy `{type:"enabled", budget_tokens}`.
///
/// **Empirical — confirm by probe** (the discipline of [`is_gemini3_level`]): Opus
/// 4.7/4.8 and Fable 5 *reject* enabled/budget and sampling outright (400); Opus 4.6 /
/// Sonnet 4.6 take adaptive too — it's the recommended shape, `budget_tokens` is
/// deprecated there. Everything older, and Haiku 4.5, stays on enabled/budget. Matched
/// by `contains` (not `starts_with`, unlike `is_gemini3_level`) so a vendor-prefixed id
/// still resolves. Add ids as they ship; a slot (or `[defaults]`) can force a tier
/// via `thinking_style`.
fn is_anthropic_adaptive(model: &str) -> bool {
    ["opus-4-6", "opus-4-7", "opus-4-8", "sonnet-4-6", "fable-5"]
        .iter()
        .any(|tier| model.contains(tier))
}

/// What one (provider, model) can perceive — and *how* an image reaches it. Capability
/// data on the same seam as [`ModelShape`], resolved per model slot: an explicit config
/// override wins, else the built-in classifier. Toolsets are assembled from resolved
/// caps (a vision model gets `view_image` when vision-in lands; a blind model never
/// sees the tool), so a capability mismatch is structural, not a runtime surprise.
///
/// The real predicate `view_image` rides on is **see ∧ transport**: a model can *see*
/// (`vision`) AND the chosen channel can *carry* the image. Anthropic/Gemini carry an
/// image inside a tool result (`tool_result_images`); OpenAI's wire forbids it (rig
/// 400s before sending), so an OpenAI VLM must receive the image on the user-turn
/// channel instead — the break-rewrite-resume path in [`run_phase`]. The two bools let
/// the toolset gate `view_image` on `vision` while the loop gates the rewrite on
/// `vision && !tool_result_images`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    /// Accepts image parts in model context (vision-in).
    pub vision: bool,
    /// Does this model's *transport* carry an image inside a tool result? When false,
    /// a seen image must ride the user-turn channel instead (see [`Arm::rewrites_view_image`]).
    pub tool_result_images: bool,
}

impl ModelCaps {
    /// Resolve the caps for `model` under `kind`. `vision_override` is the per-slot
    /// config escape hatch (`vision = true/false` in the role table) — it pins the
    /// vision answer in both directions; `None` asks the classifier. The transport
    /// channel is a property of the wire protocol alone (`kind`), never overridden.
    pub fn resolve(kind: ProviderKind, model: &str, vision_override: Option<bool>) -> Self {
        let vision = vision_override.unwrap_or_else(|| is_vision_capable(kind, model));
        Self {
            vision,
            tool_result_images: transport_supports_tool_result_images(kind),
        }
    }
}

/// Does this wire protocol carry an image *inside a tool result*? Anthropic
/// (`tool_result` image blocks) and Gemini (`functionResponse` inline data) do — it's
/// documented and first-class. The OpenAI wire forbids images in a `role:tool` message
/// and rig enforces it before sending (`ToolResultContent::Image(_) => Err(..)` in
/// `openai/completion/mod.rs`), so a `view_image` result there must instead be
/// delivered as an `image_url` part on a **user** turn. DeepSeek is moot — vision-blind,
/// so `view_image` never attaches. Branch the rewrite on *this*, not on `kind == Openai`:
/// the next no-tool-result-image provider is a table entry, not a new `if`.
fn transport_supports_tool_result_images(kind: ProviderKind) -> bool {
    match kind {
        ProviderKind::Anthropic | ProviderKind::Gemini => true,
        ProviderKind::Openai => false,
        // Vision-blind on the wire; the value is unreached (no view_image attaches),
        // but "no tool-result image channel" is the honest answer.
        ProviderKind::DeepSeek => false,
    }
}

/// The built-in vision classifier. **Empirical — confirm by probe** (the discipline
/// of [`is_anthropic_adaptive`]): boundaries reflect what the providers actually
/// serve today, and a wrong guess fails loud at the provider, not silent here.
fn is_vision_capable(kind: ProviderKind, _model: &str) -> bool {
    match kind {
        // Every current Claude completion id is multimodal-in (vision shipped with
        // Claude 3; no text-only ids remain in the lineup).
        ProviderKind::Anthropic => true,
        // The gemini-* completion line is natively multimodal across 2.x/3.x.
        ProviderKind::Gemini => true,
        // DeepSeek chat/reasoner are text-only on the wire (docs/issues.md, media
        // spine): images attached to a blind model must fail loud, not get dropped.
        ProviderKind::DeepSeek => false,
        // A generic OpenAI-compatible endpoint can front anything; vision is opt-in
        // per slot (`vision = true` in the role table) rather than guessed from an
        // arbitrary id.
        ProviderKind::Openai => false,
    }
}

/// How a given (provider, model) expresses "think" on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingStyle {
    /// Anthropic legacy: `{thinking:{type:"enabled",budget_tokens:N}}`.
    AnthropicBudget,
    /// Anthropic 4.6+: `{thinking:{type:"adaptive"}}` + `output_config.effort`.
    AnthropicAdaptive,
    /// Gemini 3-line: `generationConfig.thinkingConfig.thinkingLevel`.
    GeminiLevel,
    /// Gemini 2.5/3.5: `generationConfig.thinkingConfig.thinkingBudget`.
    GeminiBudget,
    /// DeepSeek V4 hybrids: `{thinking:{type:"enabled"}, reasoning_effort:<role>}`.
    DeepSeekEffort,
    /// No request-time toggle (the generic OpenAI path).
    None,
}

/// Where this provider's wire format puts sampling (`temperature`/`top_p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SamplingPlacement {
    /// Gemini nests it under `generationConfig` (camelCase `topP`).
    GeminiGenerationConfig,
    /// Anthropic/DeepSeek/OpenAI take it top-level (rig flattens into the body).
    TopLevel,
}

/// Force a model's thinking style, overriding the built-in classifier. `Auto` (the
/// default) classifies from the model id; the others pin a tier — the escape hatch for
/// a new or misclassified Anthropic model (see `docs/config.md`). Carried on a cast
/// slot (falling back to `[defaults]`) and resolved per arm via [`Arm::from_slot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingStyleOverride {
    #[default]
    Auto,
    Adaptive,
    Budget,
}

impl std::str::FromStr for ThinkingStyleOverride {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "adaptive" => Ok(Self::Adaptive),
            "budget" => Ok(Self::Budget),
            other => Err(anyhow!(
                "thinking_style {other:?} is not one of auto|adaptive|budget"
            )),
        }
    }
}

/// The request shape one (provider, model) wants — the unified, per-model home for
/// request tuning. [`resolve`](ModelShape::resolve) classifies it (honoring an
/// override); [`to_params`](ModelShape::to_params) emits the `additional_params` blob
/// with the per-phase budget, sampling, and effort. This is what lets a cast whose
/// explorer and synth straddle a capability line (a budget-tier Haiku explorer with an
/// adaptive Sonnet 4.6 synth, say) fit each arm correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelShape {
    thinking: ThinkingStyle,
    /// Does this model accept sampling *while thinking is on*? Anthropic 400s on a
    /// custom temperature under thinking (any tier — "temperature may only be set to 1
    /// when thinking is enabled"; 4.7/4.8/Fable reject it outright), so `false` there;
    /// DeepSeek/Gemini/OpenAI accept sampling alongside reasoning.
    sampling_under_thinking: bool,
    sampling_placement: SamplingPlacement,
}

impl ModelShape {
    /// Resolve the shape for `model` under `kind`, honoring an explicit override.
    pub fn resolve(kind: ProviderKind, model: &str, ovr: ThinkingStyleOverride) -> Self {
        let thinking = match kind {
            ProviderKind::Anthropic => {
                let adaptive = match ovr {
                    ThinkingStyleOverride::Auto => is_anthropic_adaptive(model),
                    ThinkingStyleOverride::Adaptive => true,
                    ThinkingStyleOverride::Budget => false,
                };
                if adaptive {
                    ThinkingStyle::AnthropicAdaptive
                } else {
                    ThinkingStyle::AnthropicBudget
                }
            }
            ProviderKind::Gemini => {
                if is_gemini3_level(model) {
                    ThinkingStyle::GeminiLevel
                } else {
                    ThinkingStyle::GeminiBudget
                }
            }
            ProviderKind::DeepSeek => ThinkingStyle::DeepSeekEffort,
            ProviderKind::Openai => ThinkingStyle::None,
        };
        let (sampling_under_thinking, sampling_placement) = match kind {
            ProviderKind::Anthropic => (false, SamplingPlacement::TopLevel),
            ProviderKind::Gemini => (true, SamplingPlacement::GeminiGenerationConfig),
            ProviderKind::DeepSeek | ProviderKind::Openai => (true, SamplingPlacement::TopLevel),
        };
        Self {
            thinking,
            sampling_under_thinking,
            sampling_placement,
        }
    }

    /// Just the thinking block (no sampling), with the default effort — the body of the
    /// [`thinking_params`] wrapper.
    fn thinking_only(&self, budget: u64) -> Option<Value> {
        let mut obj = serde_json::Map::new();
        self.write_thinking(&mut obj, budget, DEFAULT_EFFORT);
        (!obj.is_empty()).then_some(Value::Object(obj))
    }

    /// The full `additional_params` blob — thinking (with its effort sink where the
    /// model has one) plus sampling — or `None` when nothing is set. `effort` is the
    /// per-role depth lever; it lands where the style takes it (Anthropic adaptive
    /// → `output_config.effort`; DeepSeek → `reasoning_effort`; the Gemini 3-line
    /// → `thinkingLevel`), ignored elsewhere.
    pub fn to_params(
        &self,
        budget: u64,
        temperature: Option<f64>,
        top_p: Option<f64>,
        effort: &str,
    ) -> Option<Value> {
        let mut obj = serde_json::Map::new();
        self.write_thinking(&mut obj, budget, effort);
        let thinking_on = self.thinking != ThinkingStyle::None;

        // Drop sampling when thinking is on and this model won't accept it under
        // thinking — generalizes the Anthropic case (a custom temperature 400s under
        // thinking; thinking is the higher-value default, so it wins). DeepSeek/Gemini/
        // OpenAI accept sampling alongside reasoning, so they keep it.
        let drop_sampling = thinking_on && !self.sampling_under_thinking;
        if !drop_sampling {
            match self.sampling_placement {
                SamplingPlacement::GeminiGenerationConfig => {
                    if temperature.is_some() || top_p.is_some() {
                        let gc = obj
                            .entry("generationConfig")
                            .or_insert_with(|| json!({}))
                            .as_object_mut()
                            .expect("generationConfig is an object");
                        if let Some(t) = temperature {
                            gc.insert("temperature".into(), json!(t));
                        }
                        if let Some(p) = top_p {
                            gc.insert("topP".into(), json!(p));
                        }
                    }
                }
                SamplingPlacement::TopLevel => {
                    if let Some(t) = temperature {
                        obj.insert("temperature".into(), json!(t));
                    }
                    if let Some(p) = top_p {
                        obj.insert("top_p".into(), json!(p));
                    }
                }
            }
        }
        (!obj.is_empty()).then_some(Value::Object(obj))
    }

    /// Write this style's thinking block (and its per-role effort sink) into `obj`.
    fn write_thinking(&self, obj: &mut serde_json::Map<String, Value>, budget: u64, effort: &str) {
        match self.thinking {
            ThinkingStyle::AnthropicBudget => {
                obj.insert(
                    "thinking".into(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
            ThinkingStyle::AnthropicAdaptive => {
                obj.insert("thinking".into(), json!({ "type": "adaptive" }));
                // rig 0.34 flattens additional_params into the Messages body; its typed
                // `output_config` field models only `{format}` and stays `None` (kaibo
                // sets no output schema), so this flattened key is the only `output_config`
                // emitted. If kaibo ever adds structured output, revisit — two keys 400.
                obj.insert("output_config".into(), json!({ "effort": effort }));
            }
            ThinkingStyle::GeminiLevel => {
                // The 3-line's depth lever IS the per-role effort: the values align
                // ("high"/"low" are valid levels; the default "high" matches
                // gemini-cli's investigator, rig deserializes it to
                // ThinkingLevel::High), and like every effort it's a passthrough
                // string the provider validates. Dropping it for a hardcoded
                // "high" would silently ignore a slot's `effort = "low"`.
                obj.insert(
                    "generationConfig".into(),
                    json!({ "thinkingConfig": { "thinkingLevel": effort, "includeThoughts": true } }),
                );
            }
            ThinkingStyle::GeminiBudget => {
                obj.insert(
                    "generationConfig".into(),
                    json!({ "thinkingConfig": { "thinkingBudget": budget, "includeThoughts": true } }),
                );
            }
            ThinkingStyle::DeepSeekEffort => {
                // Explicit-on (the V4 default, but stated so intent survives a default
                // flip). rig flattens both top-level and round-trips the response
                // `reasoning_content` so tool loops don't trip DeepSeek's echo-or-400 rule.
                obj.insert("thinking".into(), json!({ "type": "enabled" }));
                obj.insert("reasoning_effort".into(), json!(effort));
            }
            ThinkingStyle::None => {}
        }
    }
}

/// Per-(provider, model) thinking params, or `None` when the model reasons without a
/// request toggle. Thin wrapper over [`ModelShape`] using the built-in classifier and
/// the default effort; the per-phase path with slot overrides goes through
/// [`Arm::from_slot`].
pub fn thinking_params(kind: ProviderKind, model: &str, budget: u64) -> Option<Value> {
    ModelShape::resolve(kind, model, ThinkingStyleOverride::Auto).thinking_only(budget)
}

/// All of a request's model-shaping params (thinking + sampling) merged into one
/// `additional_params` blob. Thin wrapper over [`ModelShape::to_params`] using the
/// built-in classifier and the default effort; the per-phase path with slot
/// overrides goes through [`Arm::from_slot`].
pub fn request_params(
    kind: ProviderKind,
    model: &str,
    budget: u64,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> Option<Value> {
    ModelShape::resolve(kind, model, ThinkingStyleOverride::Auto).to_params(
        budget,
        temperature,
        top_p,
        DEFAULT_EFFORT,
    )
}

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
        user_prompt: String,
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
        user_prompt: String,
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
            user_prompt,
            max_turns,
            params,
            progress,
            make_tools,
            break_on_view_image,
        ))
    }
}

/// One resolved phase arm: its own client + model + request params + caps. The
/// unit `consult`/`explore`/`synthesize` receive — they never learn about
/// backends or casts. The server resolves a cast's slots into arms
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
        let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);

        // One HTTP backend carrying the per-request deadline. rig exposes no
        // native timeout and its prompt loop is non-streaming, so a provider that
        // connects but never responds would hang the whole call with no other
        // brake — the 2026-06-06 wedge (~29 min; docs/issues.md). `timeout`
        // bounds a single completion; `connect_timeout` fails a dead endpoint
        // fast (capped at the deadline so a sub-10s backend timeout still
        // dominates). Injected via rig's `.http_client(..)`.
        //
        // reqwest is built `rustls-no-provider`, so `.build()` below panics unless a
        // process-default crypto provider is installed; do it now (idempotent).
        crate::tls::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(backend.request_timeout)
            .connect_timeout(backend.request_timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|e| anyhow!("http client init: {e}"))?;

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
        user_prompt: String,
        max_turns: usize,
        progress: &dyn ProgressSink,
        make_tools: ToolFactory<'_>,
    ) -> Result<String> {
        self.runner
            .run_phase(
                preamble,
                self.max_tokens,
                user_prompt,
                max_turns,
                self.params.as_ref(),
                progress,
                make_tools,
                self.rewrites_view_image(),
            )
            .await
    }
}

/// Per-call loop tunables for a phase. Model-tracking knobs (`max_tokens`, the
/// thinking budget, sampling) ride each [`Arm`] (they track the slot's model);
/// what remains here are the loop bounds the caller may dial per request, the
/// sandbox limits, and the progress sink.
#[derive(Debug, Clone)]
pub struct ConsultConfig {
    /// Bounds each cheap `explore′` sweep — it's cheap, let it rip.
    pub explorer_max_turns: usize,
    /// Bounds the recomposed consult's *whole* driver loop (it delegates sweeps AND
    /// reads spans), so it must be generous — a multi-part question blew the old 8.
    pub synth_max_turns: usize,
    /// Read-only sandbox limits applied to every kaish worker this phase spawns.
    pub sandbox: SandboxConfig,
    /// Where the phase's liveness goes: each delegated sweep and direct kaish read
    /// emits a [`PhaseEvent`] here. The server installs an adapter that renders these
    /// as MCP progress notifications when the caller asked for them; otherwise it's
    /// [`NullSink`], a no-op — so a stateless one-shot is byte-for-byte its old self.
    /// It rides on `ConsultConfig` because that's the one bundle already threaded into
    /// every phase fn and the toolset builders.
    pub progress: Arc<dyn ProgressSink>,
    /// Operator house rules (assembled `AGENTS.md` / user files) to splice into
    /// each top-level tool's preamble, or `None` for the historical bare preamble.
    /// `Arc<str>` so cloning `ConsultConfig` per phase is cheap. Rides here for the
    /// same reason as `progress`: it's the one bundle already threaded everywhere.
    /// The server fills it per call (it needs the resolved root to read the files);
    /// the `Default` is `None`, so every offline test runs the unchanged preamble.
    pub house_rules: Option<Arc<str>>,
    /// Per-phase system-prompt overrides (`[prompts]`). `Default` is empty, so the
    /// built-in preambles run unchanged. Server-set per call from the resolved
    /// config. See [`PromptOverrides`].
    pub prompts: PromptOverrides,
}

impl Default for ConsultConfig {
    fn default() -> Self {
        let d = crate::config::Defaults::default();
        Self {
            explorer_max_turns: d.explorer_max_turns,
            synth_max_turns: d.synth_max_turns,
            sandbox: SandboxConfig::default(),
            progress: Arc::new(NullSink),
            house_rules: None,
            prompts: PromptOverrides::default(),
        }
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
/// surface — `explore` ({run_kaish}), `synthesize` ({run_kaish}), and the
/// recomposed `consult` ({run_kaish, explore′}). The factory matters because of the
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
    user_prompt: String,
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
    // Loop state across view_image-break resumes. The first pass is the bare prompt
    // with no history — byte-for-byte the old single call. Each break rewrites the
    // transcript (image onto the user-turn channel) and re-enters here.
    let mut prompt: Message = Message::user(user_prompt);
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
    /// nested explorer carries the same `[prompts]`/`[context]` framing as the
    /// standalone `explore`, built once instead of per sweep.
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
                to cover breadth, and read precise spans yourself with `run_kaish`."
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
        // Reuse the one loop — explore′ is just the explorer arm's run_phase.
        // A fresh kernel per worker build (the §2.1 cost note: a KaishWorker per
        // explore′; run_phase may build a second for the turn-cap recovery turn).
        // The sub-agent's `run_kaish` carries the same sink, so its reads surface too.
        let result = self
            .arm
            .run(
                &self.preamble,
                args.question,
                self.max_turns,
                self.progress.as_ref(),
                &|| -> Result<Vec<Box<dyn ToolDyn>>> {
                    Ok(vec![Box::new(RunKaish::with_progress(
                        KaishWorker::spawn_with(&self.root, self.sandbox.clone())?,
                        self.progress.clone(),
                    ))])
                },
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

/// The `explore` unit: a cheap model drives `{run_kaish}` over `root` and returns
/// a curated report. The standalone seam behind the MCP `explore` tool — built on
/// the one loop primitive via the resolved explorer [`Arm`].
pub async fn explore(
    question: &str,
    root: impl Into<PathBuf>,
    arm: &Arm,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    arm.run(
        &phase_preamble(
            cfg.prompts.explorer.as_deref(),
            report_preamble,
            cfg.house_rules.as_deref(),
        ),
        question.to_string(),
        cfg.explorer_max_turns,
        cfg.progress.as_ref(),
        &|| phase_tools(&root, cfg, arm.caps.vision),
    )
    .await
}

/// The single-arm phase toolset (`explore`/`synthesize`): always `run_kaish`, plus
/// `view_image` when the arm's model is vision-capable. One [`KaishWorker`] backs
/// both tools (shared kernel) so a vision phase doesn't spin a second kaish thread.
fn phase_tools(root: &Path, cfg: &ConsultConfig, vision: bool) -> Result<Vec<Box<dyn ToolDyn>>> {
    let worker = KaishWorker::spawn_with(root, cfg.sandbox.clone())?;
    let mut tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(RunKaish::with_progress(
        worker.clone(),
        cfg.progress.clone(),
    ))];
    if vision {
        tools.push(Box::new(ViewImage::new(worker, root.to_path_buf())));
    }
    Ok(tools)
}

/// Build the standalone `synthesize` user prompt. Pure and offline-testable.
///
/// With `context`, frame it as trusted starting evidence — a grounded `file:line`
/// rarely needs re-deriving — and steer the model to reach for `run_kaish` when it
/// needs *more* than the context gives (a span, a whole file, a detail left open),
/// not to re-verify what's likely right (question first, then context). With no
/// context — or whitespace-only — steer the model to investigate directly via
/// `run_kaish` rather than guess, so the answer stays grounded either way.
pub fn synthesize_user_prompt(question: &str, context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!(
            "Question:\n{question}\n\n\
             Context (supplied material — typically a curated explorer report or \
             pasted source):\n{context}\n\n\
             Answer the question, grounded in concrete `file:line` evidence. The \
             context is your starting evidence — when it cites a `file:line`, trust \
             it; a grounded citation rarely needs re-deriving. Reach for the \
             `run_kaish` tool when you need more than the context gives you: a span \
             it references but doesn't quote, a whole file or a large span when you \
             need the full picture, a detail it left open, or anything the question \
             asks that it didn't cover. If the code you read and the context \
             genuinely disagree, the code wins — the code is the only ground truth. \
             Cite concrete `file:line` for every claim."
        ),
        None => format!(
            "Question:\n{question}\n\n\
             No context was supplied. Investigate the project yourself with the \
             `run_kaish` tool (a read-only kaish shell) and answer from what you find, \
             citing concrete `file:line`."
        ),
    }
}

/// Standalone synth preamble: interactive, framing supplied context as trusted
/// starting evidence and `run_kaish` as the way to *get more* when the context
/// isn't enough — including whole files and large spans. Composes the shared
/// [`kaish_syntax_core`] so the shell idioms and exit-code contract don't drift.
pub fn synthesize_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {core}\n\n\
         You may be given CONTEXT — typically a curated explorer report (a \
         SummaryOfFindings plus RelevantLocations, each with a `file:line`, key \
         symbols, and a short snippet), or pasted material. \
         Treat it as your starting evidence: when it cites a concrete `file:line`, \
         trust it — a grounded citation rarely needs re-deriving. The `run_kaish` \
         tool is yours to drive directly whenever the context isn't enough: fetch a \
         span it references but doesn't quote, read a whole file or a large span when \
         you need the full picture, chase a detail it left open, or investigate \
         anything the question reaches that the context didn't cover. Getting more \
         evidence when you need it is the normal move; you're never limited to what \
         you were handed. Where the code you read and the context genuinely disagree, \
         the code wins — the code is the only ground truth that matters. Ground every \
         claim in a concrete `file:line`."
    )
}

/// The standalone `synthesize` seam: a capable model answers `question`, grounded
/// in an optional caller-supplied `context` (typically an `explore` report or
/// pasted material), with `run_kaish` to verify or fill a precise gap. Takes the
/// resolved synth [`Arm`] — a real outside opinion, not the cheap explorer.
pub async fn synthesize(
    question: &str,
    context: Option<&str>,
    root: impl Into<PathBuf>,
    arm: &Arm,
    cfg: &ConsultConfig,
) -> Result<String> {
    let root = root.into();
    let user_prompt = synthesize_user_prompt(question, context);
    arm.run(
        &phase_preamble(
            cfg.prompts.synthesize.as_deref(),
            synthesize_preamble,
            cfg.house_rules.as_deref(),
        ),
        user_prompt,
        cfg.synth_max_turns,
        cfg.progress.as_ref(),
        &|| phase_tools(&root, cfg, arm.caps.vision),
    )
    .await
}

/// Build the consult driver's user prompt from the question and any prior session
/// turns. Pure and offline-testable: this framing is the whole of the multi-turn
/// hand-off, so it's worth pinning in a test.
///
/// With **no** history this is exactly the bare question — a stateless consult is
/// byte-for-byte unchanged. With history, prepend the prior `(question, answer)`
/// pairs as conversation context and steer the model to re-confirm any span a prior
/// answer cited: the exploration runs fresh every turn (we never replay the stored
/// report — it'd be stale), so the code is the ground truth, not the old answer.
pub fn consult_user_prompt(question: &str, history: &[QaTurn]) -> String {
    if history.is_empty() {
        return question.to_string();
    }
    let mut prompt = String::from(
        "This is a continuing conversation about the same codebase. Earlier turns, \
         oldest first:\n\n",
    );
    for (i, turn) in history.iter().enumerate() {
        prompt.push_str(&format!(
            "[Turn {}]\nQ: {}\nA: {}\n\n",
            i + 1,
            turn.question,
            turn.answer
        ));
    }
    prompt.push_str(&format!(
        "Use the earlier turns for context and continuity. Investigate fresh and \
         re-confirm any `file:line` an earlier answer cited before you rely on it — \
         the code is the ground truth, not the prior answer. Now answer the current \
         question:\n\n{question}"
    ));
    prompt
}

/// The recomposed `consult` driver: one capable model, two tools. Composes the
/// shared [`kaish_syntax_core`] (for `run_kaish`) and frames `explore` as the way
/// to cover breadth. Positive framing on purpose — weaker/local models loop on
/// blanket prohibitions, so reinforce the grounded behavior we want.
pub fn consult_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {core}\n\n\
         You also have a second tool, `explore`: it delegates a broad sweep to a \
         fast investigator that rips through the repo and reports back with a \
         curated report — RelevantLocations carrying `file:line`, key symbols, and \
         snippets. Reach for `explore` to cover breadth — find where a \
         thing lives, gather the relevant files — and use `run_kaish` to read a \
         precise span yourself and confirm a detail. Build your answer from what \
         they return: quote the key snippet, name its `file:line`, and let the \
         evidence carry the claim. Where the evidence settles the question, answer \
         it fully; where it reaches its edge, say so and name what would close the gap."
    )
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
    let worker = KaishWorker::spawn_with(root, cfg.sandbox.clone())?;
    // explore′ for delegated breadth: the same explore unit, wrapped as a tool,
    // pointed at the explorer arm — its own client, model, and request shape,
    // which may live on a different backend than the driver's. Bounded by
    // explorer_max_turns per sweep; no cap on how many times consult may delegate
    // (Amy's call — watch real behavior). Its system prompt is the same explorer
    // override-or-default + house rules the standalone `explore` resolves, built
    // once here rather than per sweep.
    let explorer_preamble: Arc<str> = Arc::from(phase_preamble(
        cfg.prompts.explorer.as_deref(),
        report_preamble,
        cfg.house_rules.as_deref(),
    ));
    let explore = RunExplore::new(
        explorer.clone(),
        cfg.explorer_max_turns,
        root,
        cfg.sandbox.clone(),
        reports,
        cfg.progress.clone(),
        explorer_preamble,
    );
    let mut tools: Vec<Box<dyn ToolDyn>> = vec![
        Box::new(RunKaish::with_progress(
            worker.clone(),
            cfg.progress.clone(),
        )),
        Box::new(explore),
    ];
    // The driver loop runs on the *synth* arm, so view_image rides the synth's
    // vision cap (the delegated explore′ sub-agent gets its own view_image keyed to
    // the explorer arm's caps, inside `explore`). Shares the driver's kernel.
    if synth_vision {
        tools.push(Box::new(ViewImage::new(worker, root.to_path_buf())));
    }
    Ok(tools)
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

    let answer = synth
        .run(
            &phase_preamble(
                cfg.prompts.consult.as_deref(),
                consult_preamble,
                cfg.house_rules.as_deref(),
            ),
            user_prompt.to_string(),
            cfg.synth_max_turns,
            cfg.progress.as_ref(),
            // Rebuilt per call (main loop, and again if run_phase forces a final
            // turn); every build shares the one `reports` sink so all explore′
            // sweeps aggregate.
            &|| consult_tools(explorer, root, cfg, reports.clone(), synth.caps.vision),
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
    root: &Path,
    explorer: &Arm,
    synth: &Arm,
    cfg: &ConsultConfig,
) -> Result<ConsultOutput> {
    let history = match session {
        Some((store, id)) => store.history(id),
        None => Vec::new(),
    };
    let user_prompt = consult_user_prompt(question, &history);

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
    root: impl Into<PathBuf>,
    explorer: &Arm,
    synth: &Arm,
    cfg: &ConsultConfig,
    session: Option<Session<'_>>,
) -> Result<ConsultOutput> {
    let root = root.into();
    consult_session_turn(session, question, &root, explorer, synth, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;
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
        const MODEL: &str = "explorer";
        const MARKER: &str = "HOUSE_RULE_MARKER: prefer tabs over spaces";

        let dir = tempdir().unwrap();

        // Configured → the marker and its framing ride in the preamble.
        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        let cfg = ConsultConfig {
            house_rules: Some(Arc::from(MARKER)),
            ..ConsultConfig::default()
        };
        explore("q", dir.path(), &arm(&client, MODEL), &cfg)
            .await
            .unwrap();
        let reqs = client.requests_for(MODEL);
        let pre = reqs[0].preamble.as_deref().unwrap_or("");
        assert!(
            pre.contains(MARKER),
            "house rules must reach the preamble: {pre}"
        );
        assert!(
            pre.contains("Operator house rules"),
            "the framing header must introduce them: {pre}"
        );
        // Still the explorer's own role framing — house rules append, not replace.
        assert!(
            pre.contains("code explorer"),
            "base preamble must remain: {pre}"
        );

        // Unconfigured → the same call carries the base preamble and no marker.
        let bare = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        let cfg2 = ConsultConfig::default(); // house_rules: None
        explore("q", dir.path(), &arm(&bare, MODEL), &cfg2)
            .await
            .unwrap();
        let reqs2 = bare.requests_for(MODEL);
        let pre2 = reqs2[0].preamble.as_deref().unwrap_or("");
        assert!(!pre2.contains(MARKER), "no [context] → no marker: {pre2}");
        assert!(
            pre2.contains("code explorer"),
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
            house_rules: Some(Arc::from(MARKER)),
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
    /// still append on top — `[prompts]` and `[context]` are orthogonal axes. We
    /// drive a single `explore` phase and read back what the model was handed.
    #[tokio::test]
    async fn a_prompt_override_fully_replaces_the_preamble_and_house_rules_still_append() {
        const MODEL: &str = "explorer";
        const CUSTOM: &str = "You are a SECURITY AUDITOR. Hunt injection sinks.";
        const HOUSE: &str = "HOUSE_RULE_MARKER: prefer tabs";

        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig {
            prompts: PromptOverrides {
                explorer: Some(CUSTOM.to_string()),
                ..PromptOverrides::default()
            },
            house_rules: Some(Arc::from(HOUSE)),
            ..ConsultConfig::default()
        };
        explore("q", dir.path(), &arm(&client, MODEL), &cfg)
            .await
            .unwrap();

        let reqs = client.requests_for(MODEL);
        let pre = reqs[0].preamble.as_deref().unwrap_or("");
        // The override is verbatim...
        assert!(pre.contains(CUSTOM), "override prose missing: {pre}");
        // ...the built-in framing is fully replaced (full-replace, by decision)...
        assert!(
            !pre.contains("code explorer"),
            "override must REPLACE, not augment, the built-in: {pre}"
        );
        // ...and house rules still layer on top.
        assert!(pre.contains(HOUSE), "house rules must still append: {pre}");
    }

    /// Each phase reads its *own* override key — an `explorer`-only override must
    /// not bleed into the `synthesize` phase, which keeps its built-in. Guards the
    /// per-phase routing in [`phase_preamble`]/[`PromptOverrides`].
    #[tokio::test]
    async fn prompt_overrides_are_per_phase() {
        const MODEL: &str = "synth";
        const CUSTOM_EXPLORER: &str = "EXPLORER_ONLY_OVERRIDE";

        let client = ScriptedClient::builder()
            .on_model(MODEL, |_req| Ok(text_response("done")))
            .build();
        let dir = tempdir().unwrap();
        // Only the explorer key is set; the synthesize phase must ignore it.
        let cfg = ConsultConfig {
            prompts: PromptOverrides {
                explorer: Some(CUSTOM_EXPLORER.to_string()),
                ..PromptOverrides::default()
            },
            ..ConsultConfig::default()
        };
        synthesize("q", None, dir.path(), &arm(&client, MODEL), &cfg)
            .await
            .unwrap();

        let pre = client.requests_for(MODEL)[0]
            .preamble
            .clone()
            .unwrap_or_default();
        assert!(
            !pre.contains(CUSTOM_EXPLORER),
            "the explorer override must not bleed into synthesize: {pre}"
        );
        // synthesize keeps its built-in framing.
        assert!(
            pre.contains("You answer a question about a codebase"),
            "synthesize keeps its built-in preamble: {pre}"
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
                        "t-rg",
                        "run_kaish",
                        json!({ "script": "rg -n target_marker src" }),
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
                        "t-rg",
                        "run_kaish",
                        json!({ "script": "rg -n target_marker src" }),
                    ))
                } else {
                    Ok(text_response("EXPLORER_REPORT: src/foo.rs:1"))
                }
            })
            .build();

        let dir = project_with_marker();
        let sink = Arc::new(RecordingSink::default());
        let cfg = ConsultConfig {
            progress: sink.clone(),
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
            events.contains(&PhaseEvent::KaishRun { script: "rg -n target_marker src".into() }),
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
            script: "rg -n target_marker src".into(),
        });
        let finish = pos(&PhaseEvent::SweepFinished);
        assert!(
            start < nested && nested < finish,
            "sweep must bracket its nested read: {events:?}"
        );
    }

    /// A stateless consult (default `ConsultConfig`) emits to the [`NullSink`] — no
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

    /// The Gemini 3-line's depth lever IS the per-role effort: a slot's `effort`
    /// must land as `thinkingLevel` (the values align — "high"/"low" are valid
    /// levels), not be silently dropped into a hardcoded max-depth. A user
    /// setting `effort = "low"` on a gemini-3 slot means it.
    #[test]
    fn gemini_level_takes_the_per_role_effort_as_its_thinking_level() {
        let shape = ModelShape::resolve(
            ProviderKind::Gemini,
            "gemini-3-pro-preview",
            ThinkingStyleOverride::Auto,
        );
        let params = shape.to_params(8192, None, None, "low").unwrap();
        assert_eq!(
            params["generationConfig"]["thinkingConfig"]["thinkingLevel"], "low",
            "the slot's effort is the level"
        );
        // The default effort keeps today's wire shape byte-for-byte.
        let params = shape.to_params(8192, None, None, DEFAULT_EFFORT).unwrap();
        assert_eq!(
            params["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
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

    /// The explorer preamble carries the behaviors we measured into it — the
    /// whole-file reading directive (the lite-explorer win, 48→23 turns), the
    /// context-buffer `rg` idiom, and the three report sections the synth side now
    /// expects. Pure and offline; pins the prose so a future edit can't silently
    /// drop any of it (the synth preambles are written against this shape).
    #[test]
    fn report_preamble_keeps_the_reading_directive_and_report_shape() {
        let p = report_preamble();
        // Reading strategy: read whole files, locate with an rg context buffer.
        assert!(p.contains("cat -n FILE"), "whole-file read idiom: {p}");
        assert!(
            p.to_lowercase().contains("whole"),
            "the whole-file directive must survive: {p}"
        );
        assert!(p.contains("rg -n -B4 -A8"), "rg context-buffer idiom: {p}");
        // The report template the synth (synthesize/consult preambles) is written
        // against — keep the three section names in lockstep with those.
        for section in ["SummaryOfFindings", "RelevantLocations", "ExplorationTrace"] {
            assert!(p.contains(section), "missing report section {section}: {p}");
        }
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

    /// The single-arm phase toolset (`explore`/`synthesize`): `run_kaish` always,
    /// `view_image` exactly when the arm is vision-capable. Pins the shared helper
    /// both ways so neither phase's gate drifts.
    #[test]
    fn phase_tools_gates_view_image_on_the_vision_cap() {
        let dir = tempdir().unwrap();
        let cfg = ConsultConfig::default();

        let blind = phase_tools(dir.path(), &cfg, false).expect("blind toolset builds");
        let blind_names: Vec<String> = blind.iter().map(|t| t.name()).collect();
        assert!(blind_names.iter().any(|n| n == "run_kaish"));
        assert!(
            !blind_names.iter().any(|n| n == "view_image"),
            "no view_image without vision, got {blind_names:?}"
        );

        let seeing = phase_tools(dir.path(), &cfg, true).expect("vision toolset builds");
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

    /// No session history ⇒ the prompt is *exactly* the bare question. This pins the
    /// promise that a stateless consult is byte-for-byte its pre-session behavior.
    #[test]
    fn empty_history_yields_the_bare_question() {
        assert_eq!(
            consult_user_prompt("Where is the sandbox enforced?", &[]),
            "Where is the sandbox enforced?"
        );
    }

    /// With history, every prior turn appears, the current question appears, and the
    /// turns precede the current question (the model reads context before the ask).
    #[test]
    fn history_is_replayed_before_the_current_question_in_order() {
        let history = vec![
            QaTurn::new("What is kaish?", "A read-only shell (src/sandbox.rs)."),
            QaTurn::new("Who calls it?", "consult drives it (src/consult.rs)."),
        ];
        let prompt = consult_user_prompt("And explore?", &history);

        for needle in [
            "What is kaish?",
            "A read-only shell (src/sandbox.rs).",
            "Who calls it?",
            "consult drives it (src/consult.rs).",
            "And explore?",
        ] {
            assert!(
                prompt.contains(needle),
                "prompt must carry {needle:?}:\n{prompt}"
            );
        }
        // Ordering: the first prior turn comes before the second, and both come
        // before the current question.
        let first = prompt.find("What is kaish?").unwrap();
        let second = prompt.find("Who calls it?").unwrap();
        let current = prompt.find("And explore?").unwrap();
        assert!(first < second, "turns must be oldest-first");
        assert!(
            second < current,
            "history must precede the current question"
        );
    }
}
