//! Provider-drift and request-shaping knowledge for the consult phases.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::credentials::ProviderKind;

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
        // slot (`vision = true`), like the generic OpenAI kind; the built-in openrouter
        // cast pins it on both its (multimodal) default models.
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
    /// Gemini 3-line: `generationConfig.thinkingConfig.thinkingLevel`.
    GeminiLevel,
    /// Gemini 2.5/3.5: `generationConfig.thinkingConfig.thinkingBudget`.
    GeminiBudget,
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
            ProviderKind::Gemini => {
                if is_gemini3_level(model) {
                    ThinkingStyle::GeminiLevel
                } else {
                    ThinkingStyle::GeminiBudget
                }
            }
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

    /// Does this shape have a sink for a `thinking_budget` tunable? Only the two
    /// explicit-budget styles emit it (`budget_tokens` / `thinkingBudget`); the
    /// effort-driven styles (Anthropic adaptive, the Gemini 3-line, DeepSeek) and the
    /// toggle-less openai path ignore a budget entirely. Lets the `kaibo://config`
    /// render flag a per-slot `thinking_budget` that will never leave the process.
    pub fn sinks_thinking_budget(&self) -> bool {
        matches!(
            self.thinking,
            ThinkingStyle::AnthropicBudget | ThinkingStyle::GeminiBudget
        )
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
            ThinkingStyle::OpenRouterEffort => {
                // The unified reasoning knob. `effort` is the per-role depth lever, a
                // passthrough string OpenRouter validates against its ladder
                // (none|minimal|low|medium|high|xhigh|max) — so the default "high" maps
                // to "high", and a slot's `effort = "xhigh"` reaches the deeper rungs
                // without a code change. The gateway maps it onto each upstream model's
                // native reasoning field, or drops it for a model that has none.
                obj.insert("reasoning".into(), json!({ "effort": effort }));
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
}
