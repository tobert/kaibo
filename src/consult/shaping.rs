//! Provider-drift and request-shaping knowledge for the consult phases.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::config::DataCollection;
use crate::credentials::ProviderKind;

/// Token budget for model "thinking"/reasoning, for the providers that expose a
/// request-time toggle. Sized well under [`ConsultConfig`]'s `max_tokens` so the
/// reasoning never starves the actual answer (a thinking model that spends its
/// whole budget reasoning returns empty content — we saw exactly that on Gemma).
/// Anthropic additionally *requires* `max_tokens > budget_tokens`.
pub const THINKING_BUDGET: u64 = 8192;

/// The per-role thinking-depth lever for the models that expose one as a request
/// param (Anthropic adaptive's `output_config.effort`, DeepSeek's `reasoning_effort`).
/// A passthrough string — like a model id — so a rung a provider ships tomorrow
/// lands without a kaibo release. Default for both roles unless a slot or the
/// per-role `[defaults]` tunes it.
///
/// **kaibo keeps no allowlist.** Two of the six wire paths *do* have a closed ladder,
/// but it belongs to rig's typed request builder, not to the provider — see
/// [`EffortWire`] and [`accepted_efforts`], which read that ladder back out of rig
/// itself rather than restating it here.
pub const DEFAULT_EFFORT: &str = "high";

/// The reasoning **depths** kaibo knows how to order, shallow → deep. Used for exactly
/// one thing: comparing two efforts when a lane needs a floor
/// ([`batch_effort`](crate::batch::batch_effort)). It is a ranking, never a gate — an
/// effort outside this list is still shaped and sent, and a comparison against it simply
/// defers to the operator's value.
///
/// **[`EFFORT_OFF`] is deliberately not on this ladder.** "Off" is not the shallowest
/// depth; it is a different kind of answer. Ranking it as rung 0 let a *depth* floor
/// silently switch reasoning back **on** — see [`EFFORT_OFF`] and [`is_effort_off`],
/// which are the other half of this pair. Anything that ranks an effort has to decide
/// what it does about the off-switch first.
pub const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The reasoning **opt-out** — a sentinel beside [`EFFORT_LADDER`], not a rung on it.
///
/// Two providers ship a structural off-switch (DeepSeek's `thinking:{"type":"disabled"}`,
/// OpenRouter's `reasoning:{"enabled":false}`) and both mishandle a "zero effort" string
/// riding beside an explicit enable — DeepSeek bills 160–253 reasoning tokens for exactly
/// that pairing (probed 2026-08-01). So `none` is answered by *shape*, not by value:
/// [`ModelShape::to_params`] routes it through [`is_effort_off`].
///
/// Keeping it off the ladder is the same distinction one level up. A floor that raises
/// *depth* (batch) must leave an operator's "off" alone, because turning reasoning back
/// on isn't "more depth" — it's ignoring the setting, and billing them for it.
pub const EFFORT_OFF: &str = "none";

/// Where `effort` sits on [`EFFORT_LADDER`], or `None` for anything that isn't an
/// orderable depth: [`EFFORT_OFF`] (ask [`is_effort_off`] instead — off is not a depth),
/// a provider's newer rung, or an operator's typo. The last two look identical from here
/// and both mean "don't second-guess it". Case- and whitespace-insensitive.
pub fn effort_rank(effort: &str) -> Option<usize> {
    let e = effort.trim().to_ascii_lowercase();
    EFFORT_LADDER.iter().position(|rung| *rung == e)
}

/// Is this effort the reasoning opt-out ([`EFFORT_OFF`])? The counterpart to
/// [`effort_rank`]: that one orders depths and returns `None` here; this one answers the
/// question ranking can't. **Public because the batch lane must ask it** — a depth floor
/// consulting only the ladder would see `none` as unrankable, and an earlier version that
/// ranked it as rung 0 lifted it to `BATCH_EFFORT`, quietly billing reasoning on every
/// item of a fan-out the operator had switched off.
///
/// Case- and whitespace-insensitive, so `"None"` can never quietly mean "on".
pub fn is_effort_off(effort: &str) -> bool {
    effort.trim().eq_ignore_ascii_case(EFFORT_OFF)
}

/// Every effort spelling kaibo knows by name — the off-switch, then the depth ladder.
/// For *probing* and *presenting* ("what does this wire accept?"), never for gating: an
/// effort absent from this list is still shaped and sent.
pub fn known_efforts() -> Vec<&'static str> {
    std::iter::once(EFFORT_OFF).chain(EFFORT_LADDER).collect()
}

/// Which Anthropic models want **adaptive** thinking (`{type:"adaptive"}` plus an
/// `output_config.effort`) instead of the legacy `{type:"enabled", budget_tokens}`.
///
/// **Empirical — confirm by probe**: Opus 4.7/4.8 and Fable 5 *reject* enabled/budget
/// and sampling outright (400); Opus 4.6 / Sonnet 4.6 take adaptive too — it's the
/// recommended shape, `budget_tokens` is deprecated there. Everything older, and Haiku
/// 4.5, stays on enabled/budget. Matched by `contains` so a vendor-prefixed id still
/// resolves. Add ids as they ship; a slot (or `[defaults]`) can force a tier via
/// `thinking_style`.
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
        // OpenRouter speaks the OpenAI wire but is *more* dangerous than a 400: rig's
        // converter silently rewrites a tool-result image to the placeholder text
        // "[Image content not supported in tool results]" (openrouter/completion.rs,
        // the `ToolResultContent::Image(_)` arm of the `role:tool` conversion) — a
        // quiet drop, the exact silent loss kaibo refuses. `false` routes a seen image
        // onto the user-turn channel (the break-rewrite-resume path) *before* it can
        // reach that converter, so the bytes actually arrive.
        ProviderKind::OpenRouter => false,
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
        // OpenRouter fronts every model — vision-capable and text-only alike — so the
        // capability is a property of the pinned *model*, not the gateway. Opt in per
        // slot (`vision = true`), like the generic OpenAI kind. The built-in openrouter
        // cast splits by role: its explorer default (multimodal `qwen3.6-flash`) pins
        // vision on, while its synth default (text-only `qwen3.7-max`) leaves the slot
        // `None` and lets this false stand.
        ProviderKind::OpenRouter => false,
    }
}

/// How a given (provider, model) expresses "think" on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingStyle {
    /// Anthropic legacy: `{thinking:{type:"enabled",budget_tokens:N}}`.
    AnthropicBudget,
    /// Anthropic 4.6+: `{thinking:{type:"adaptive"}}` + `output_config.effort`.
    AnthropicAdaptive,
    /// Gemini (3.x+): `generationConfig.thinkingConfig.thinkingLevel`. kaibo supports
    /// the Gemini 3-line and newer only — the whole line takes a level; the legacy
    /// 2.5-era `thinkingBudget` knob is not modeled (a pre-3.x id would emit a level
    /// and fail loud at the provider, the intended "unsupported" signal).
    GeminiLevel,
    /// DeepSeek V4 hybrids: `{thinking:{type:"enabled"}, reasoning_effort:<role>}`.
    DeepSeekEffort,
    /// OpenRouter's unified reasoning param: `{reasoning:{effort:<role>}}`. The gateway
    /// translates it per upstream provider (Anthropic budget, OpenAI effort, Gemini
    /// thinkingLevel) and silently drops it for a model that has no reasoning knob, so
    /// emitting it unconditionally is safe.
    OpenRouterEffort,
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
            // kaibo targets the Gemini 3-line and newer, whose whole family takes
            // `thinkingLevel` (Google's current thinking doc lists level for 3-pro /
            // 3-flash / 3.1-pro / 3.5-flash and drops `thinkingBudget` entirely — that
            // knob is the retired 2.5-era shape). So every Gemini id routes to level;
            // the per-role effort rides it. A pre-3.x id fails loud, not silent.
            ProviderKind::Gemini => ThinkingStyle::GeminiLevel,
            ProviderKind::DeepSeek => ThinkingStyle::DeepSeekEffort,
            ProviderKind::OpenRouter => ThinkingStyle::OpenRouterEffort,
            ProviderKind::Openai => ThinkingStyle::None,
        };
        let (sampling_under_thinking, sampling_placement) = match kind {
            ProviderKind::Anthropic => (false, SamplingPlacement::TopLevel),
            ProviderKind::Gemini => (true, SamplingPlacement::GeminiGenerationConfig),
            // OpenRouter takes OpenAI-shaped sampling top-level and, being tolerant of
            // unsupported params (dropped per-model), keeps it under reasoning.
            ProviderKind::DeepSeek | ProviderKind::OpenRouter | ProviderKind::Openai => {
                (true, SamplingPlacement::TopLevel)
            }
        };
        Self {
            thinking,
            sampling_under_thinking,
            sampling_placement,
        }
    }

    /// Does this shape have a sink for a `thinking_budget` tunable? Only the
    /// Anthropic legacy-budget style emits one (`budget_tokens`); every other style —
    /// the effort-driven ones (Anthropic adaptive, Gemini's `thinkingLevel`, DeepSeek,
    /// OpenRouter) and the toggle-less openai path — ignores a budget entirely. Lets the
    /// `kaibo://config` render flag a per-slot `thinking_budget` that will never leave
    /// the process.
    pub fn sinks_thinking_budget(&self) -> bool {
        matches!(self.thinking, ThinkingStyle::AnthropicBudget)
    }

    /// Does this shape have a sink for an `effort` tunable? Only the effort-driven
    /// styles route it (`output_config.effort` / `thinkingLevel` / `reasoning_effort`);
    /// the budget styles and the openai path drop it. Counterpart to
    /// [`sinks_thinking_budget`](Self::sinks_thinking_budget) for the render's no-op flag.
    pub fn sinks_effort(&self) -> bool {
        matches!(
            self.thinking,
            ThinkingStyle::AnthropicAdaptive
                | ThinkingStyle::GeminiLevel
                | ThinkingStyle::DeepSeekEffort
                | ThinkingStyle::OpenRouterEffort
        )
    }

    /// Does this shape actually send sampling (`temperature`/`top_p`)? Kaibo runs
    /// thinking on by default, and a model that rejects sampling under thinking
    /// (Anthropic, any tier) has it dropped — so a per-slot `temperature` there is
    /// inert. The toggle-less openai path (`thinking == None`) keeps sampling, as do
    /// Gemini/DeepSeek. Mirrors the drop in [`to_params`](Self::to_params).
    pub fn sinks_sampling(&self) -> bool {
        self.thinking == ThinkingStyle::None || self.sampling_under_thinking
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
                // Gemini's depth lever IS the per-role effort: the values align (Google's
                // thinking doc lists `minimal|low|medium|high` across the 3-line; the
                // default "high" deserializes to rig's `ThinkingLevel::High` and matches
                // gemini-cli's investigator). Dropping it for a hardcoded "high" would
                // silently ignore a slot's `effort = "low"`.
                //
                // Unlike the other effort sinks this one is NOT a passthrough string: rig
                // 0.38 models the level as a closed enum (`minimal|low|medium|high`), so
                // `xhigh`/`max`/`none` are refused by rig's own converter before the
                // request leaves the process. That ceiling is rig's, not Google's —
                // [`EffortWire::Gemini`] names it and `Arm::from_slot` turns it into a
                // legible error instead of a mid-call surprise.
                obj.insert(
                    "generationConfig".into(),
                    json!({ "thinkingConfig": { "thinkingLevel": effort, "includeThoughts": true } }),
                );
            }
            ThinkingStyle::DeepSeekEffort => {
                // `"none"` gets the *structural* off-switch. Measured on deepseek-v4-pro
                // (2026-08-01): `type:"enabled"` + `reasoning_effort:"none"` bills 160–253
                // reasoning tokens — the explicit enable wins and the opt-out is a silent
                // no-op the operator pays for. `type:"disabled"` alone is a true zero.
                if is_effort_off(effort) {
                    obj.insert("thinking".into(), json!({ "type": "disabled" }));
                } else {
                    // Explicit-on (the V4 default, but stated so intent survives a default
                    // flip). rig flattens both top-level and round-trips the response
                    // `reasoning_content` so tool loops don't trip DeepSeek's echo-or-400
                    // rule.
                    obj.insert("thinking".into(), json!({ "type": "enabled" }));
                    obj.insert("reasoning_effort".into(), json!(effort));
                }
            }
            ThinkingStyle::OpenRouterEffort => {
                // The unified reasoning knob. `effort` is the per-role depth lever, a
                // passthrough string OpenRouter validates against its ladder
                // (minimal|low|medium|high|xhigh|max) — so the default "high" maps
                // to "high", and a slot's `effort = "xhigh"` reaches the deeper rungs
                // without a code change. The gateway maps it onto each upstream model's
                // native reasoning field, or drops it for a model that has none.
                // `"none"` is the opt-out and gets the *structural* disable — the
                // gateway's documented off-switch — rather than trusting the effort
                // ladder to treat the literal string as off (a drift there would
                // silently re-enable reasoning, billed as output tokens).
                if is_effort_off(effort) {
                    obj.insert("reasoning".into(), json!({ "enabled": false }));
                } else {
                    obj.insert("reasoning".into(), json!({ "effort": effort }));
                }
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

/// Additional params for hosted OpenAI through rig's Responses API client.
///
/// This is deliberately *not* the generic `ProviderKind::Openai` shape. Local
/// OpenAI-compatible servers and gateways commonly implement only
/// `/chat/completions`, so they stay on [`ThinkingStyle::None`]. A backend whose
/// resolved base URL is OpenAI Platform gets this first-party Responses shape.
/// Reasoning and sampling are independent model-family capabilities: current GPT
/// reasoning models (`gpt-5*`, including `gpt-5.6-sol`) take `reasoning.effort`
/// and reject custom `temperature`; older GPT chat families (`gpt-4*`,
/// `gpt-3.5*`, `chatgpt-*`) take sampling and reject `reasoning.effort`.
///
/// *Whether* a model takes reasoning is a family question; **which rungs it takes is a
/// per-model question** — measured 2026-08-01, the ladder moves at both ends within
/// `gpt-5*`: `gpt-5.6` reaches `max`, `gpt-5.2` stops at `xhigh`, `gpt-5.1` at `high`,
/// and plain `gpt-5`'s bottom rung is `minimal` where 5.1+ have `none` instead. kaibo
/// deliberately encodes none of that: an operator gets to reach a rung OpenAI shipped
/// this morning, and a wrong one comes back as OpenAI's own error naming the model's
/// supported values. (rig's enum is a separate, *lower* ceiling that kaibo does report
/// up front — see [`EffortWire`].)
pub fn hosted_openai_responses_params(
    model: &str,
    top_p: Option<f64>,
    effort: &str,
) -> Option<Value> {
    let mut obj = serde_json::Map::new();
    if hosted_openai_accepts_reasoning(model) {
        obj.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if hosted_openai_accepts_sampling(model) {
        if let Some(p) = top_p {
            obj.insert("top_p".into(), json!(p));
        }
    }
    (!obj.is_empty()).then_some(Value::Object(obj))
}

/// The typed temperature rig's Responses client should receive for hosted OpenAI.
/// `None` means "use the provider default". This is intentionally conservative:
/// current GPT reasoning models reject `temperature`; known older chat families
/// accept it. Unknown hosted model IDs omit sampling until a live probe proves the
/// shape.
pub fn hosted_openai_responses_temperature(model: &str, temperature: f64) -> Option<f64> {
    hosted_openai_accepts_sampling(model).then_some(temperature)
}

pub fn hosted_openai_accepts_sampling(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("gpt-4") || m.starts_with("gpt-3.5") || m.starts_with("chatgpt-")
}

pub fn hosted_openai_accepts_reasoning(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("gpt-5")
}

/// Does this (backend, slot) actually put the per-role `effort` on the wire? The one
/// answer both the wire path and the `kaibo://config` render read, so an operator's
/// "is my `effort` doing anything?" is settled in exactly one place. `responses_wire`
/// is the backend's resolved interactive wire ([`Backend::uses_responses_wire`]),
/// which is what splits a hosted GPT-5 slot (reasoning) from the same `openai` kind
/// pointed at a local llama.cpp server (no reasoning knob at all).
pub fn effort_sinks(
    kind: ProviderKind,
    model: &str,
    style: ThinkingStyleOverride,
    responses_wire: bool,
) -> bool {
    if responses_wire {
        return hosted_openai_accepts_reasoning(model);
    }
    ModelShape::resolve(kind, model, style).sinks_effort()
}

/// Which of rig's request builders this arm's `additional_params` blob has to survive.
///
/// kaibo hands rig a JSON blob; most providers `#[serde(flatten)]` it straight into the
/// body, so whatever an operator writes reaches the provider and the *provider* decides.
/// Two paths don't: rig deserializes the blob into a typed struct first, and a typed
/// struct has a closed set of reasoning rungs. That ceiling belongs to **rig's client**,
/// not the provider's API — OpenAI accepts `effort = "max"` on the wire (probed live
/// 2026-08-01, 200 + echoed) while rig 0.38's `ReasoningEffort` enum stops at `xhigh`.
///
/// Naming the wire lets kaibo ask rig *before* the call (see [`preflight_params`]) and
/// report the ceiling honestly instead of surfacing a bare serde message mid-consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortWire {
    /// `gemini::completion::create_request_body` parses the blob as
    /// `gemini_api_types::AdditionalParameters`, whose `ThinkingLevel` is a closed enum.
    Gemini,
    /// OpenAI's Responses builder parses the blob as `responses_api::AdditionalParameters`,
    /// whose `ReasoningEffort` is a closed enum.
    OpenaiResponses,
    /// rig flattens the blob into the body verbatim (Anthropic, DeepSeek, OpenRouter, and
    /// the generic OpenAI `/chat/completions` shape). kaibo can express anything here; who
    /// answers for it is the *provider's* business, and they differ — DeepSeek validates
    /// strictly against its own seven rungs, OpenRouter accepts all seven and normalizes
    /// each onto the upstream's native knob (proven 2026-08-01: `xhigh`/`max` reach
    /// `openai/gpt-5.1` through the gateway with reasoning billed, on ids that 400 those
    /// rungs on OpenAI's direct API), and the toggle-less chat shape has no field to put
    /// it in at all. "Passthrough" is a statement about *rig*, not about the far end.
    Passthrough,
}

impl EffortWire {
    /// The wire an arm built from `kind` (with its resolved `responses_wire` flag) runs on.
    pub fn resolve(kind: ProviderKind, responses_wire: bool) -> Self {
        if responses_wire {
            return Self::OpenaiResponses;
        }
        match kind {
            ProviderKind::Gemini => Self::Gemini,
            ProviderKind::Anthropic
            | ProviderKind::DeepSeek
            | ProviderKind::OpenRouter
            | ProviderKind::Openai => Self::Passthrough,
        }
    }

    /// How an operator would name this wire in a config conversation.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenaiResponses => "openai responses",
            Self::Passthrough => "passthrough",
        }
    }
}

/// Run `params` through rig's own converter for `wire`, exactly as the live call will.
/// `Ok(())` means the request builder accepts this blob; `Err` carries rig's own message.
///
/// This is a *preflight*, not a policy: it asks rig, it doesn't hold a list. A rung rig
/// starts accepting after a version bump starts working here the same day, with no kaibo
/// change — which is the whole reason effort stayed a passthrough string.
pub fn preflight_params(
    wire: EffortWire,
    params: Option<&Value>,
) -> std::result::Result<(), String> {
    let Some(params) = params else {
        return Ok(());
    };
    match wire {
        EffortWire::Passthrough => Ok(()),
        // The exact `serde_json::from_value` rig runs in `create_request_body`
        // (gemini/completion.rs) — same type, same failure.
        EffortWire::Gemini => serde_json::from_value::<
            rig_core::providers::gemini::completion::gemini_api_types::AdditionalParameters,
        >(params.clone())
        .map(|_| ())
        .map_err(|e| e.to_string()),
        // The exact `serde_json::from_value` rig runs building a Responses request
        // (openai/responses_api/mod.rs).
        EffortWire::OpenaiResponses => serde_json::from_value::<
            rig_core::providers::openai::responses_api::AdditionalParameters,
        >(params.clone())
        .map(|_| ())
        .map_err(|e| e.to_string()),
    }
}

/// The effort values `wire` accepts today, read back out of rig by probing each of
/// [`known_efforts`] through [`preflight_params`]. Derived, never declared — so the list
/// an error message shows an operator is rig's current truth even after a rig bump, and
/// kaibo never grows a second ladder to drift from it.
pub fn accepted_efforts(wire: EffortWire) -> Vec<&'static str> {
    if wire == EffortWire::Passthrough {
        return known_efforts();
    }
    known_efforts()
        .into_iter()
        .filter(|rung| {
            let probe = match wire {
                EffortWire::Gemini => {
                    json!({ "generationConfig": { "thinkingConfig": { "thinkingLevel": rung } } })
                }
                _ => json!({ "reasoning": { "effort": rung } }),
            };
            preflight_params(wire, Some(&probe)).is_ok()
        })
        .collect()
}

/// Fold this arm's output-token budget into `params` where the provider needs it
/// carried out-of-band. **OpenRouter only, and it's a rig-defect workaround**: rig
/// 0.38's `OpenrouterCompletionRequest` (openrouter/completion.rs) has no `max_tokens`
/// field and its `TryFrom` never reads `CompletionRequest.max_tokens`, so
/// `AgentBuilder::max_tokens()` is silently a no-op for that provider — the answer
/// would run on OpenRouter's own default budget, starving a thinking-on completion.
/// `additional_params` *is* `#[serde(flatten)]`-merged into the body, so we inject the
/// budget there under `max_completion_tokens` (OpenRouter's preferred spelling; the
/// spec deprecates `max_tokens`). A no-op for every other kind — rig sends their
/// `max_tokens` natively, so a second copy here would be redundant at best.
pub fn inject_output_budget(
    kind: ProviderKind,
    params: Option<Value>,
    max_tokens: u64,
) -> Option<Value> {
    if kind != ProviderKind::OpenRouter {
        return params;
    }
    let mut obj = match params {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("max_completion_tokens".into(), json!(max_tokens));
    Some(Value::Object(obj))
}

/// Fold the backend's upstream-host data policy into `params`. **OpenRouter only**:
/// one slug routes across competing hosts whose data policies differ, and kaibo's
/// prompts carry source code — so under [`DataCollection::Deny`] (the default) every
/// request pins `provider: {"data_collection": "deny"}` and OpenRouter routes only
/// to no-collection hosts (a model with no compliant host fails loudly, which is
/// the point). The explicit `Allow` opt-in emits nothing: kaibo never pushes
/// *toward* collection, it just steps aside and lets the operator's own OpenRouter
/// account settings govern. A no-op for every other kind.
pub fn inject_provider_prefs(
    kind: ProviderKind,
    params: Option<Value>,
    data_collection: DataCollection,
) -> Option<Value> {
    if kind != ProviderKind::OpenRouter || data_collection == DataCollection::Allow {
        return params;
    }
    let mut obj = match params {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("provider".into(), json!({ "data_collection": "deny" }));
    Some(Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every Gemini slot kaibo actually ships routes the per-role effort as
    /// `thinkingLevel` — including the ones a nominal `gemini-3-…` substring test would
    /// miss: the `-latest` aliases the built-in casts pin
    /// (`gemini-flash-lite-latest`/`gemini-pro-latest`/`gemini-flash-latest`, no
    /// `gemini-3` substring at all) and the concrete 3.x previews with a *dot* after the
    /// 3 (`gemini-3.1-pro-preview`, `gemini-3.5-flash`). Google's thinking doc lists
    /// `thinkingLevel` across the whole 3-line, so it's the native depth knob and the
    /// sink the effort lever rides. The classifier must reach all of them; a shape that
    /// dropped effort into a fixed `thinkingBudget` would silently ignore a slot's
    /// `effort = "low"`. This asserts the effort reaches the wire as the level, and no
    /// budget rides along.
    #[test]
    fn configured_gemini_ids_take_the_effort_as_thinking_level() {
        // The literal ids kaibo classifies from (config.rs `default_models` + the
        // built-in `gemini-batch` cast), plus the concrete 3.x previews they resolve to.
        let ids = [
            "gemini-flash-lite-latest", // built-in Gemini explorer default
            "gemini-3.5-flash",         // built-in gemini synth default
            "gemini-pro-latest",        // built-in gemini-batch synth
            "gemini-flash-latest",      // a -latest alias with no version in the id
            "gemini-3.1-pro-preview",   // a dotted 3.x id the old dash test missed
        ];
        for id in ids {
            let shape = ModelShape::resolve(ProviderKind::Gemini, id, ThinkingStyleOverride::Auto);
            assert!(
                shape.sinks_effort(),
                "{id}: a Gemini slot must route the per-role effort, not drop it"
            );
            assert!(
                !shape.sinks_thinking_budget(),
                "{id}: Gemini has no budget sink — a per-slot thinking_budget is inert"
            );
            let params = shape.to_params(8192, None, None, "low").unwrap();
            let cfg = &params["generationConfig"]["thinkingConfig"];
            assert_eq!(
                cfg["thinkingLevel"], "low",
                "{id}: the slot's effort must land as thinkingLevel"
            );
            assert!(
                cfg.get("thinkingBudget").is_none(),
                "{id}: Gemini takes the level, not a fixed budget"
            );
        }
    }

    /// OpenRouter's unified reasoning knob: the per-role effort must land as
    /// `{reasoning:{effort:<role>}}` — thinking-on by default, and a slot's `effort`
    /// (including OpenRouter's deeper `xhigh`/`max` rungs, which pass through as any
    /// other string) reaching the gateway, not silently dropped.
    #[test]
    fn openrouter_reasoning_carries_the_per_role_effort() {
        let shape = ModelShape::resolve(
            ProviderKind::OpenRouter,
            "~anthropic/claude-sonnet-latest",
            ThinkingStyleOverride::Auto,
        );
        let params = shape.to_params(8192, None, None, DEFAULT_EFFORT).unwrap();
        assert_eq!(
            params["reasoning"]["effort"], "high",
            "the default effort rides the unified reasoning param"
        );
        // A deeper rung passes through verbatim — xhigh/max are reachable via slot config.
        let params = shape.to_params(8192, None, None, "xhigh").unwrap();
        assert_eq!(params["reasoning"]["effort"], "xhigh");
        // Sampling stays top-level and survives alongside reasoning (OpenRouter drops
        // it per-model if unsupported, so kaibo emits it).
        let params = shape
            .to_params(8192, Some(0.5), None, DEFAULT_EFFORT)
            .unwrap();
        assert_eq!(params["temperature"], 0.5);
    }

    /// `effort = "none"` on DeepSeek must be the *structural* disable
    /// (`thinking:{"type":"disabled"}`), not enabled-plus-`reasoning_effort:"none"`.
    ///
    /// Measured against deepseek-v4-pro (live probe, 2026-08-01), one variable at a time:
    /// `reasoning_effort:"none"` alone → 0 reasoning tokens; `thinking:{"type":"disabled"}`
    /// → 0 reasoning tokens; **`thinking:{"type":"enabled"}` + `reasoning_effort:"none"`
    /// → reasoning ON, 160–253 tokens billed.** The explicit `type:"enabled"` wins, so the
    /// old shape turned an operator's opt-out into a silent no-op they paid for — the exact
    /// silent-fallback class kaibo refuses. Same structural-off pattern as OpenRouter below.
    #[test]
    fn deepseek_effort_none_disables_reasoning_structurally() {
        let shape = ModelShape::resolve(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            ThinkingStyleOverride::Auto,
        );
        let params = shape.to_params(8192, None, None, "none").unwrap();
        assert_eq!(
            params["thinking"]["type"], "disabled",
            "none must turn thinking off structurally, not ask for zero effort while \
             leaving it enabled: {params}"
        );
        assert!(
            params.get("reasoning_effort").is_none(),
            "no effort string rides alongside the disable — it is what re-enabled \
             reasoning: {params}"
        );
        // Off is off however it was spelled; every other rung still rides as effort.
        assert_eq!(
            shape.to_params(8192, None, None, "NONE").unwrap()["thinking"]["type"],
            "disabled"
        );
        let params = shape.to_params(8192, None, None, "low").unwrap();
        assert_eq!(params["thinking"]["type"], "enabled");
        assert_eq!(params["reasoning_effort"], "low");
    }

    /// `effort = "none"` is the documented reasoning opt-out, and it must be the
    /// *structural* disable (`{"reasoning": {"enabled": false}}`), not a passthrough
    /// effort string — OpenRouter's explicit off-switch, independent of how the
    /// gateway's effort ladder treats the literal "none" today. Any other rung
    /// keeps riding as effort (pinned above).
    #[test]
    fn openrouter_effort_none_disables_reasoning_structurally() {
        let shape = ModelShape::resolve(
            ProviderKind::OpenRouter,
            "~anthropic/claude-sonnet-latest",
            ThinkingStyleOverride::Auto,
        );
        let params = shape.to_params(8192, None, None, "none").unwrap();
        assert_eq!(
            params["reasoning"]["enabled"], false,
            "none must emit the explicit disable, not rely on ladder semantics"
        );
        assert!(
            params["reasoning"].get("effort").is_none(),
            "no effort string rides alongside the disable"
        );
    }

    /// Hosted OpenAI uses rig's Responses client, whose native shape is
    /// `reasoning.effort` plus typed `max_output_tokens` (from core max_tokens).
    /// It is intentionally separate from the generic OpenAI-compatible
    /// `/chat/completions` shape, which stays toggle-less for local servers.
    #[test]
    fn hosted_openai_responses_params_are_model_aware_about_sampling() {
        let params = hosted_openai_responses_params("gpt-5.6-sol", Some(0.95), DEFAULT_EFFORT)
            .expect("hosted OpenAI sends Responses params");
        assert_eq!(
            params["reasoning"]["effort"], "high",
            "the per-role effort reaches OpenAI Responses"
        );
        assert!(
            params.get("top_p").is_none(),
            "current GPT reasoning models get no sampling knobs"
        );
        assert!(
            hosted_openai_responses_temperature("gpt-5.6-sol", 0.4).is_none(),
            "current GPT reasoning models reject temperature"
        );
        assert!(
            params.get("max_tokens").is_none() && params.get("max_completion_tokens").is_none(),
            "Responses maps core max_tokens to max_output_tokens itself"
        );

        let params = hosted_openai_responses_params("gpt-4.1-mini", Some(0.95), "low")
            .expect("older chat GPT families keep sampling");
        assert!(
            params.get("reasoning").is_none(),
            "older chat GPT families reject reasoning.effort"
        );
        assert_eq!(
            params["top_p"], 0.95,
            "known sampling-compatible GPT families keep top_p"
        );
        assert_eq!(
            hosted_openai_responses_temperature("gpt-4.1-mini", 0.4),
            Some(0.4),
            "known sampling-compatible GPT families keep typed temperature"
        );

        let params = hosted_openai_responses_params("gpt-5.6-sol", None, "none").unwrap();
        assert_eq!(
            params["reasoning"]["effort"], "none",
            "the explicit no-reasoning effort passes through to rig's Responses enum"
        );
        assert!(params.get("top_p").is_none());

        assert!(
            hosted_openai_responses_params("unknown-openai-model", Some(0.95), "high").is_none(),
            "unknown hosted models get no guessed provider-specific params"
        );
    }

    /// Kaibo's prompts carry source code, and one OpenRouter slug routes across
    /// hosts with differing data policies — so every OpenRouter request must pin
    /// `provider.data_collection = "deny"` unless the backend explicitly opted in.
    /// The deny must merge into the existing params blob (reasoning, budget,
    /// sampling) without clobbering it, and the Allow opt-in must emit *nothing* —
    /// kaibo steps aside for the account's own settings, it never pushes toward
    /// collection. Other kinds are untouched either way.
    #[test]
    fn openrouter_denies_data_collection_by_default() {
        let params = ModelShape::resolve(
            ProviderKind::OpenRouter,
            "z-ai/glm-5.2",
            ThinkingStyleOverride::Auto,
        )
        .to_params(8192, None, None, DEFAULT_EFFORT);
        let out = inject_provider_prefs(ProviderKind::OpenRouter, params, DataCollection::Deny)
            .expect("openrouter always sends params");
        assert_eq!(
            out["provider"]["data_collection"], "deny",
            "the default must pin no-collection routing on every request"
        );
        assert_eq!(
            out["reasoning"]["effort"], "high",
            "the deny merges beside the reasoning blob, not over it"
        );

        // The explicit opt-in: no provider object at all.
        let params = ModelShape::resolve(
            ProviderKind::OpenRouter,
            "z-ai/glm-5.2",
            ThinkingStyleOverride::Auto,
        )
        .to_params(8192, None, None, DEFAULT_EFFORT);
        let out = inject_provider_prefs(ProviderKind::OpenRouter, params, DataCollection::Allow)
            .expect("openrouter always sends params");
        assert!(
            out.get("provider").is_none(),
            "allow emits nothing — the account's own policy governs"
        );

        // Any other kind: passthrough both ways, including a None params.
        let out = inject_provider_prefs(ProviderKind::Anthropic, None, DataCollection::Deny);
        assert!(out.is_none(), "non-openrouter kinds are untouched");
    }

    /// OpenRouter drops rig's native `max_tokens`, so the budget must ride
    /// `additional_params` as `max_completion_tokens` — the rig-defect workaround. The
    /// value must actually land in the blob the arm sends; and it must be a no-op for
    /// every other kind (rig sends their `max_tokens` natively).
    #[test]
    fn openrouter_output_budget_rides_max_completion_tokens() {
        // Merges into an existing reasoning blob without clobbering it.
        let params = ModelShape::resolve(
            ProviderKind::OpenRouter,
            "~google/gemini-flash-latest",
            ThinkingStyleOverride::Auto,
        )
        .to_params(8192, None, None, DEFAULT_EFFORT);
        let out = inject_output_budget(ProviderKind::OpenRouter, params, 16384).unwrap();
        assert_eq!(
            out["max_completion_tokens"], 16384,
            "the output budget must reach the request body OpenRouter reads"
        );
        assert_eq!(
            out["reasoning"]["effort"], "high",
            "the injection preserves the reasoning param"
        );

        // Even with no other params (None), the budget still lands.
        let out = inject_output_budget(ProviderKind::OpenRouter, None, 4096).unwrap();
        assert_eq!(out["max_completion_tokens"], 4096);

        // A no-op for every other kind — rig sends their max_tokens itself.
        assert!(inject_output_budget(ProviderKind::Anthropic, None, 4096).is_none());
        let anthropic = ModelShape::resolve(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            ThinkingStyleOverride::Auto,
        )
        .to_params(8192, None, None, DEFAULT_EFFORT);
        let passthrough = inject_output_budget(ProviderKind::Anthropic, anthropic.clone(), 16384);
        assert_eq!(
            passthrough, anthropic,
            "non-OpenRouter params pass through untouched — no max_completion_tokens added"
        );
    }

    /// Canary tying `inject_output_budget` to the rig 0.38 pin. The workaround
    /// exists because rig's `OpenrouterCompletionRequest` has no `max_tokens`
    /// field; the day a rig bump adds one, `AgentBuilder::max_tokens` starts
    /// arriving natively and the injected `max_completion_tokens` rides alongside
    /// it — redundant at best, a provider 400 at worst, and silent either way.
    /// This failing test is the tripwire: on a rig bump, re-read rig's
    /// `openrouter/completion.rs`, retire (or deliberately keep) the injection,
    /// then advance the version prefix here.
    #[test]
    fn rig_bump_reaudits_the_openrouter_budget_workaround() {
        let lock = include_str!("../../Cargo.lock");
        let entry = lock
            .find("name = \"rig-core\"")
            .expect("rig-core pinned in Cargo.lock");
        let version = lock[entry..]
            .lines()
            .nth(1)
            .expect("version line follows the name line");
        assert!(
            version.contains("version = \"0.38."),
            "rig-core moved past 0.38 ({version}): re-audit the \
             OpenrouterCompletionRequest max_tokens defect before shipping — \
             see inject_output_budget"
        );
    }
}
