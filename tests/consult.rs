//! Consult over resolved arms: offline prompt/shape tests, a mixed-cast routing
//! e2e over two scripted clients, and the `#[ignore]`d live probes.

use std::sync::{Arc, Mutex};

use kaibo::config::{default_models, Config, Defaults, ModelRole, ModelSlot};
use kaibo::consult::{
    consult, consult_user_prompt, effort_sinks, hosted_openai_responses_params, oneshot,
    request_params, thinking_params, Arm, ConsultConfig, ModelCaps, ModelShape, PhaseContext,
    ThinkingStyleOverride, DEFAULT_EFFORT, THINKING_BUDGET,
};
use kaibo::credentials::{load, ProviderKind};
use serde_json::{json, Value};

/// Resolve one of `cast`'s slots into a live arm — the same resolution the server
/// performs, for the live tests below. Constructs a real rig client (and so
/// resolves the backend's key), which is why only the `#[ignore]`d tests call it.
fn arm_from(cfg: &Config, cast: &str, role: ModelRole) -> Arm {
    let cast = cfg
        .resolve_cast(cast)
        .unwrap_or_else(|e| panic!("resolve cast: {e}"));
    let slot = cast
        .require_slot(role)
        .unwrap_or_else(|e| panic!("slot: {e}"));
    let backend = cfg
        .resolve_backend(&slot.backend)
        .unwrap_or_else(|e| panic!("backend: {e}"));
    Arm::from_slot(backend, slot, role, &cfg.defaults).unwrap_or_else(|e| panic!("arm: {e}"))
}

/// A built-in cast's arm by name, for the live tests below.
fn builtin_arm(cast: &str, role: ModelRole) -> Arm {
    arm_from(&Config::builtin(), cast, role)
}

/// A throwaway explorer arm for the driver-only `view_image` tests. `consult` needs
/// an explorer arm to assemble its toolset, but these tests never make the driver
/// delegate a sweep, so it is never invoked — it just has to exist. Shares the
/// synth's scripted client under a model id the test never scripts.
fn unused_explorer(client: &ScriptedClient) -> Arm {
    Arm::new(
        client.clone(),
        "unused-explorer",
        16384,
        None,
        ModelCaps {
            vision: false,
            tool_result_images: true,
        },
    )
}

#[test]
fn consult_seed_context_is_framed_as_trusted_evidence() {
    // The seam that absorbed standalone `synthesize`: caller `context` rides the
    // consult driver's prompt as *trusted starting evidence*, with the steer to
    // investigate for more — not to re-verify what's likely right.
    let p = consult_user_prompt(
        "What blocks writes?",
        Some("src/sandbox.rs:95 read-only mount"),
        &[],
        &[],
    );

    assert!(p.contains("What blocks writes?"), "question present");
    assert!(
        p.contains("src/sandbox.rs:95 read-only mount"),
        "context present"
    );
    assert!(p.to_lowercase().contains("context"), "context labelled");
    // Framing: a grounded citation is trusted; tools are for *getting more* when the
    // context isn't enough — not for re-verifying. Pin both halves so a revert toward
    // "verify the cited span", or one that drops the get-more license, fails here.
    let lower = p.to_lowercase();
    assert!(
        lower.contains("trust"),
        "a grounded citation should be trusted, got: {p}"
    );
    assert!(
        lower.contains("more than the context gives you")
            || lower.contains("that the context does not"),
        "supplied context must steer toward fetching more, not re-verifying, got: {p}"
    );
}

#[test]
fn consult_prompt_without_context_or_history_is_the_bare_question() {
    // No seed, no thread ⇒ the prompt is exactly the question; the investigation
    // steer lives in the consult preamble, not this prompt. Whitespace-only context
    // is no context — don't pretend there's evidence.
    assert_eq!(
        consult_user_prompt("What blocks writes?", None, &[], &[]),
        "What blocks writes?"
    );
    assert_eq!(
        consult_user_prompt("Q?", Some("  \n  "), &[], &[]),
        "Q?",
        "blank context should behave like None"
    );
}

#[test]
fn thinking_is_enabled_for_kinds_with_a_request_toggle() {
    // Anthropic is model-aware: Haiku 4.5 (and older) take the legacy enabled/budget
    // block; Sonnet 4.6 (and newer) take adaptive thinking. Both via a top-level block
    // rig flattens into the Messages request.
    let budget = thinking_params(ProviderKind::Anthropic, "claude-haiku-4-5", THINKING_BUDGET)
        .expect("anthropic budget-tier has a thinking toggle");
    assert_eq!(budget["thinking"]["type"], "enabled");
    assert_eq!(budget["thinking"]["budget_tokens"], THINKING_BUDGET);

    let adaptive = thinking_params(
        ProviderKind::Anthropic,
        "claude-sonnet-4-6",
        THINKING_BUDGET,
    )
    .expect("anthropic adaptive-tier has a thinking toggle");
    assert_eq!(adaptive["thinking"]["type"], "adaptive");
    assert!(
        adaptive["thinking"].get("budget_tokens").is_none(),
        "adaptive carries no budget"
    );

    // Gemini (3.x+): nested under generationConfig.thinkingConfig with camelCase keys —
    // rig parses these into a typed GenerationConfig, so the shape must be exact. The
    // whole 3-line takes a level; the default effort rides it as thinkingLevel.
    let g = thinking_params(ProviderKind::Gemini, "gemini-3.5-flash", THINKING_BUDGET)
        .expect("gemini has a thinking toggle");
    assert_eq!(
        g["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    assert_eq!(
        g["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}

#[test]
fn every_gemini_id_takes_thinking_level_not_budget() {
    // kaibo targets the Gemini 3-line and newer, whose whole family uses `thinkingLevel`
    // (mutually exclusive with the retired 2.5-era budget); rig's typed ThinkingConfig
    // serializes the level as snake_case "high". Every Gemini id kaibo ships must route
    // there — the dash 3-line, the dotted 3.x previews, and the `-latest` aliases whose
    // literal id carries no version at all.
    for id in [
        "gemini-3-pro-preview",     // dash 3-line
        "gemini-3.1-pro-preview",   // dotted 3.x preview
        "gemini-3.5-flash",         // built-in gemini synth default
        "gemini-flash-lite-latest", // built-in explorer default (a -latest alias)
        "gemini-pro-latest",        // built-in gemini-batch synth (a -latest alias)
    ] {
        let g = thinking_params(ProviderKind::Gemini, id, THINKING_BUDGET).expect("toggle");
        let tc = &g["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingLevel"], "high", "{id} wants a level");
        assert!(
            tc.get("thinkingBudget").is_none(),
            "{id}: level and budget are mutually exclusive"
        );
        assert_eq!(tc["includeThoughts"], true, "{id} includes thoughts");
    }
}

#[test]
fn thinking_budget_is_threaded_through_not_hardcoded() {
    // A per-slot budget must reach the request, not the old global constant. The one
    // remaining budget sink is legacy-tier Anthropic (Haiku 4.5) — the adaptive tier
    // carries no budget, and Gemini takes a level not a budget — so Haiku is what
    // exercises budget threading.
    let a = thinking_params(ProviderKind::Anthropic, "claude-haiku-4-5", 4096)
        .expect("anthropic toggle");
    assert_eq!(a["thinking"]["budget_tokens"], 4096);
}

#[test]
fn request_params_places_sampling_where_each_wire_format_wants_it() {
    // Gemini: temperature + topP (camelCase) ride under generationConfig, alongside
    // the thinkingConfig — one merged blob, not two.
    let g = request_params(
        ProviderKind::Gemini,
        "gemini-3.5-flash",
        THINKING_BUDGET,
        Some(0.1),
        Some(0.95),
    )
    .expect("gemini params");
    let gc = &g["generationConfig"];
    assert_eq!(gc["temperature"], 0.1);
    assert_eq!(gc["topP"], 0.95, "Gemini uses camelCase topP");
    assert_eq!(
        gc["thinkingConfig"]["thinkingLevel"], "high",
        "thinking still rides along"
    );
    assert!(
        g.get("temperature").is_none(),
        "must not leak to top level for Gemini"
    );

    // Anthropic: thinking wins — sampling is dropped (any tier). The Messages API 400s on
    // any `temperature != 1` (and restricts top_p/top_k) whenever thinking is enabled
    // ("temperature may only be set to 1 when thinking is enabled"). Thinking is the
    // higher-value default (AGENTS.md "Driving the models"), so we keep it and let the
    // per-role sampling go inert rather than 400 the request. (Budget tier here; the
    // adaptive tier drops sampling the same way — see anthropic_newest_models_*.)
    let a = request_params(
        ProviderKind::Anthropic,
        "claude-haiku-4-5",
        4096,
        Some(0.3),
        Some(0.95),
    )
    .expect("anthropic params");
    assert_eq!(a["thinking"]["budget_tokens"], 4096);
    assert!(
        a.get("temperature").is_none(),
        "temperature must not ride alongside thinking for Anthropic"
    );
    assert!(
        a.get("top_p").is_none(),
        "top_p must not ride alongside thinking for Anthropic"
    );

    // OpenAI: no thinking toggle, but sampling still goes top-level.
    let o = request_params(
        ProviderKind::Openai,
        "Gemma-4-E4B-it-GGUF",
        0,
        Some(0.2),
        Some(0.9),
    )
    .expect("openai sampling");
    assert_eq!(o["temperature"], 0.2);
    assert_eq!(o["top_p"], 0.9);
    assert!(o.get("thinking").is_none());

    // DeepSeek: thinking toggle and sampling coexist, all top-level.
    let d = request_params(
        ProviderKind::DeepSeek,
        "deepseek-v4-pro",
        THINKING_BUDGET,
        Some(0.3),
        Some(0.95),
    )
    .expect("deepseek params");
    assert_eq!(d["thinking"]["type"], "enabled");
    assert_eq!(d["reasoning_effort"], "high");
    assert_eq!(d["temperature"], 0.3);
    assert_eq!(d["top_p"], 0.95);

    // No sampling, no budget — but the openai wire still asks for reasoning, so the
    // blob is not empty. `reasoning_effort` alone is the whole of it.
    let o = request_params(ProviderKind::Openai, "m", 0, None, None)
        .expect("the openai wire carries reasoning_effort even with no sampling set");
    assert_eq!(o["reasoning_effort"], "high");
    assert_eq!(
        o.as_object().map(|m| m.len()),
        Some(1),
        "nothing rides along beside it: {o}"
    );

    // Reasoning off is what empties it — then the leaf passes no additional_params.
    assert!(
        ModelShape::resolve(ProviderKind::Openai, "m", ThinkingStyleOverride::Off)
            .to_params(0, None, None, DEFAULT_EFFORT)
            .is_none(),
        "thinking_style = off leaves nothing to send"
    );
}

#[test]
fn anthropic_thinking_suppresses_sampling_or_the_api_400s() {
    // Regression for the live 400: "temperature may only be set to 1 when thinking is
    // enabled". Anthropic rejects custom temperature (and restricts top_p/top_k) while
    // extended thinking is on. Thinking is on for every Anthropic slot by default,
    // and temperature has a default on every slot, so the per-role sampling feature
    // collides with thinking on the very first call. Thinking wins; sampling drops.
    // (Budget tier shown; the adaptive tier drops sampling identically.)
    let a = request_params(
        ProviderKind::Anthropic,
        "claude-haiku-4-5",
        THINKING_BUDGET,
        Some(0.3),
        Some(0.95),
    )
    .expect("anthropic still sends the thinking block");
    assert_eq!(a["thinking"]["type"], "enabled");
    assert_eq!(a["thinking"]["budget_tokens"], THINKING_BUDGET);
    assert!(
        a.get("temperature").is_none(),
        "temperature would 400 alongside thinking"
    );
    assert!(
        a.get("top_p").is_none(),
        "top_p is dropped with it, for the same reason"
    );

    // The suppression is keyed on whether the model accepts sampling under thinking, not
    // on the provider alone: contrast DeepSeek, which also enables thinking but *accepts*
    // sampling, so its knobs ride through untouched.
    let d = request_params(
        ProviderKind::DeepSeek,
        "deepseek-v4-pro",
        THINKING_BUDGET,
        Some(0.3),
        Some(0.95),
    )
    .expect("deepseek params");
    assert_eq!(
        d["temperature"], 0.3,
        "DeepSeek keeps sampling even with thinking on"
    );
    assert_eq!(d["top_p"], 0.95);
}

#[test]
fn anthropic_newest_models_get_adaptive_thinking() {
    // Opus 4.7/4.8 and Fable 5 *require* adaptive: enabled/budget AND temperature/top_p
    // all 400. Each gets `{thinking:{type:adaptive}, output_config:{effort}}` and no
    // sampling. (Mirrors the live "fix anthropic" goal — these would 400 on the old shape.)
    for model in ["claude-opus-4-8", "claude-opus-4-7", "claude-fable-5"] {
        let a = request_params(
            ProviderKind::Anthropic,
            model,
            THINKING_BUDGET,
            Some(0.3),
            Some(0.95),
        )
        .unwrap_or_else(|| panic!("{model} sends a thinking block"));
        assert_eq!(a["thinking"]["type"], "adaptive", "{model} wants adaptive");
        assert!(
            a["thinking"].get("budget_tokens").is_none(),
            "{model} rejects budget_tokens"
        );
        assert_eq!(
            a["output_config"]["effort"], DEFAULT_EFFORT,
            "{model} carries effort"
        );
        assert!(
            a.get("temperature").is_none(),
            "{model} rejects temperature"
        );
        assert!(a.get("top_p").is_none(), "{model} rejects top_p");
    }
}

#[test]
fn anthropic_classifier_boundary() {
    // Pin the empirical boundary so a drift is a failing test, not a silent 400.
    let thinking_type = |model: &str| {
        request_params(ProviderKind::Anthropic, model, THINKING_BUDGET, None, None)
            .unwrap_or_else(|| panic!("{model} sends a thinking block"))["thinking"]["type"]
            .as_str()
            .expect("thinking.type is a string")
            .to_string()
    };
    for model in [
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-fable-5",
    ] {
        assert_eq!(
            thinking_type(model),
            "adaptive",
            "{model} is the adaptive tier"
        );
    }
    for model in [
        "claude-haiku-4-5",
        "claude-opus-4-5",
        "claude-sonnet-4-5",
        "claude-opus-4-1",
    ] {
        assert_eq!(
            thinking_type(model),
            "enabled",
            "{model} stays on enabled/budget"
        );
    }
}

#[test]
fn effort_is_per_role_and_provider_mapped() {
    // Effort lands only where the model takes it, at that provider's wire field.
    let anthropic = ModelShape::resolve(
        ProviderKind::Anthropic,
        "claude-opus-4-8",
        ThinkingStyleOverride::Auto,
    )
    .to_params(THINKING_BUDGET, None, None, "xhigh")
    .expect("adaptive params");
    assert_eq!(anthropic["output_config"]["effort"], "xhigh");

    let deepseek = ModelShape::resolve(
        ProviderKind::DeepSeek,
        "deepseek-v4-pro",
        ThinkingStyleOverride::Auto,
    )
    .to_params(THINKING_BUDGET, None, None, "max")
    .expect("deepseek params");
    assert_eq!(deepseek["reasoning_effort"], "max");

    // Gemini's wire field for effort is thinkingLevel, nested in generationConfig — a
    // level ("low"), not the reasoning_effort/output_config the other providers use.
    let gemini = ModelShape::resolve(
        ProviderKind::Gemini,
        "gemini-3.5-flash",
        ThinkingStyleOverride::Auto,
    )
    .to_params(THINKING_BUDGET, None, None, "low")
    .expect("gemini params");
    assert_eq!(
        gemini["generationConfig"]["thinkingConfig"]["thinkingLevel"], "low",
        "Gemini effort rides thinkingLevel"
    );
    assert!(
        gemini.get("reasoning_effort").is_none() && gemini.get("output_config").is_none(),
        "Gemini uses its own field, not another provider's"
    );

    // Budget-tier Anthropic (Haiku 4.5) has no effort sink — the value is ignored, not leaked.
    let budget = ModelShape::resolve(
        ProviderKind::Anthropic,
        "claude-haiku-4-5",
        ThinkingStyleOverride::Auto,
    )
    .to_params(THINKING_BUDGET, None, None, "xhigh")
    .expect("budget params");
    assert!(
        budget.get("output_config").is_none(),
        "budget tier has no effort sink"
    );

    // Per-role resolution through the slot fallback: with no per-slot override, the
    // explorer and synth arms of the *same* slot resolve their own [defaults] effort
    // (the seam `Arm::from_slot` reads — Dialect dissolved into per-arm shaping).
    let d = Defaults {
        explorer_effort: "low".into(),
        synth_effort: "max".into(),
        ..Default::default()
    };
    let slot = ModelSlot::bare("anthropic", "claude-opus-4-8");

    let et = slot.tunables(ModelRole::Explorer, &d);
    let st = slot.tunables(ModelRole::Synth, &d);
    let explorer = ModelShape::resolve(ProviderKind::Anthropic, &slot.id, et.thinking_style)
        .to_params(
            et.thinking_budget,
            Some(et.temperature),
            Some(et.top_p),
            &et.effort,
        )
        .expect("explorer params");
    let synth = ModelShape::resolve(ProviderKind::Anthropic, &slot.id, st.thinking_style)
        .to_params(
            st.thinking_budget,
            Some(st.temperature),
            Some(st.top_p),
            &st.effort,
        )
        .expect("synth params");
    assert_eq!(
        explorer["output_config"]["effort"], "low",
        "explorer effort"
    );
    assert_eq!(synth["output_config"]["effort"], "max", "synth effort");
}

#[test]
fn thinking_style_override_forces_a_tier() {
    // The escape hatch both ways: force an older model onto adaptive, or a newest model
    // back onto budget, when the classifier is wrong or a new id ships.
    let forced_adaptive = ModelShape::resolve(
        ProviderKind::Anthropic,
        "claude-sonnet-4-5",
        ThinkingStyleOverride::Adaptive,
    )
    .to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT)
    .expect("forced adaptive");
    assert_eq!(
        forced_adaptive["thinking"]["type"], "adaptive",
        "override beats the classifier"
    );

    let forced_budget = ModelShape::resolve(
        ProviderKind::Anthropic,
        "claude-opus-4-8",
        ThinkingStyleOverride::Budget,
    )
    .to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT)
    .expect("forced budget");
    assert_eq!(
        forced_budget["thinking"]["type"], "enabled",
        "override beats the classifier"
    );
    assert_eq!(forced_budget["thinking"]["budget_tokens"], THINKING_BUDGET);
}

#[test]
fn deepseek_v4_toggles_thinking_with_reasoning_effort() {
    // DeepSeek V4 is a request-time hybrid: top-level thinking + reasoning_effort
    // (rig flattens additional_params into the body, so top-level is where they go).
    // The budget is irrelevant here — depth is an effort level, not a token count.
    let d = thinking_params(ProviderKind::DeepSeek, "deepseek-v4-pro", THINKING_BUDGET)
        .expect("deepseek v4 toggles thinking at request time");
    assert_eq!(d["thinking"]["type"], "enabled");
    assert_eq!(d["reasoning_effort"], "high");
    assert!(
        d.get("generationConfig").is_none(),
        "DeepSeek is not Gemini-shaped"
    );
}

#[test]
fn the_generic_openai_path_asks_for_reasoning_by_default() {
    // Reasoning is ON by default wherever a model can do it. `reasoning_effort` is the
    // spelling the OpenAI-compatible field settled on, so kaibo sends it to any such
    // endpoint rather than leaving a reasoning model running thin.
    let p = thinking_params(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", THINKING_BUDGET)
        .expect("the openai-compatible wire carries reasoning_effort");
    assert_eq!(p["reasoning_effort"], "high");
    // One key, alone. DeepSeek pairs `reasoning_effort` with a `thinking` object, and
    // borrowing that pairing is what a strict gateway refuses outright — Crusoe answers
    // `403 Request blocked: parameter 'thinking' is not allowed`, failing the call
    // instead of ignoring the field.
    assert!(
        p.get("thinking").is_none(),
        "no `thinking` block on this wire — a strict gateway 403s the whole request: {p}"
    );
    assert!(p.get("reasoning").is_none(), "that shape is OpenRouter's: {p}");
}

/// The off-switch on the **Responses** wire, checked where the request is really built.
///
/// The sibling test below calls `ModelShape::to_params` directly, and that is exactly
/// why it missed this: on a Responses-wire backend the engine *discards* that blob and
/// replaces it with `hosted_openai_responses_params`, so a `to_params`-level assertion
/// proves nothing about what gets sent. The first version of this feature shipped with
/// `thinking_style = "off"` silently ignored on every hosted gpt-5 slot, green tests and
/// all — found by a cross-family review reading the call site instead of the unit.
#[test]
fn thinking_style_off_reaches_the_responses_wire_too() {
    // Reasoning is on for this model by default...
    let on = hosted_openai_responses_params("gpt-5.6-sol", None, "high", ThinkingStyleOverride::Auto)
        .expect("a gpt-5 model takes reasoning");
    assert_eq!(on["reasoning"]["effort"], "high");

    // ...and `off` must remove it here, not merely in the shape the engine throws away.
    let off = hosted_openai_responses_params("gpt-5.6-sol", None, "high", ThinkingStyleOverride::Off);
    assert!(
        off.as_ref().and_then(|v| v.get("reasoning")).is_none(),
        "thinking_style = off must silence the Responses wire, got {off:?}"
    );

    // And the reporting has to agree, or kaibo would say effort ships while it does not.
    assert!(
        !effort_sinks(
            ProviderKind::Openai,
            "gpt-5.6-sol",
            ThinkingStyleOverride::Off,
            true,
        ),
        "with reasoning off, the Responses wire has no effort sink to report"
    );
}

#[test]
fn thinking_style_off_sends_no_reasoning_parameter_on_any_wire() {
    // The escape hatch for a server that rejects unknown fields instead of ignoring
    // them. It has to answer for every wire, not just the openai one, because the
    // operator is saying "send nothing" — not "pick a different dialect".
    for (kind, model) in [
        (ProviderKind::Openai, "Gemma-4-E4B-it-GGUF"),
        (ProviderKind::DeepSeek, "deepseek-v4-pro"),
        (ProviderKind::Anthropic, "claude-sonnet-4-6"),
        (ProviderKind::Gemini, "gemini-3.5-flash"),
        (ProviderKind::OpenRouter, "z-ai/glm-5.2"),
    ] {
        let shape = ModelShape::resolve(kind, model, ThinkingStyleOverride::Off);
        let p = shape.to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT);
        let has_reasoning = p.as_ref().is_some_and(|v| {
            ["thinking", "reasoning", "reasoning_effort", "output_config"]
                .iter()
                .any(|k| v.get(k).is_some())
                || v.pointer("/generationConfig/thinkingConfig").is_some()
        });
        assert!(
            !has_reasoning,
            "{kind:?}/{model}: thinking_style = off must send no reasoning parameter, got {p:?}"
        );
    }
}

#[test]
fn default_headroom_sits_large_above_the_thinking_budget() {
    // Amy's default: few high-value turns, generous output budget. The headroom now
    // rides each arm (seeded from [defaults] via the slot fallback), not ConsultConfig.
    let d = Defaults::default();
    assert!(
        d.max_tokens >= 16384,
        "want generous headroom, got {}",
        d.max_tokens
    );
    // Anthropic requires max_tokens strictly greater than the thinking budget.
    assert!(d.max_tokens > THINKING_BUDGET);
    // And a bare slot inherits exactly that headroom — the value an Arm carries.
    let t = ModelSlot::bare("anthropic", "claude-fable-5").tunables(ModelRole::Synth, &d);
    assert_eq!(
        t.max_tokens, d.max_tokens,
        "the slot fallback is [defaults]"
    );
    assert!(t.max_tokens > t.thinking_budget);
}

#[test]
fn model_caps_classify_per_kind_and_honor_the_override() {
    // Capability data on the same seam as ModelShape: classified per (kind,
    // model), with an explicit config override as the escape hatch. Boundaries
    // are empirical — confirm a new model with a live probe, don't guess.

    // Every current Anthropic and Gemini completion id is multimodal-in.
    assert!(ModelCaps::resolve(ProviderKind::Anthropic, "claude-sonnet-4-6", None).vision);
    assert!(ModelCaps::resolve(ProviderKind::Anthropic, "claude-haiku-4-5", None).vision);
    assert!(ModelCaps::resolve(ProviderKind::Gemini, "gemini-3.5-flash", None).vision);
    // DeepSeek chat/reasoner are text-only on the wire.
    assert!(!ModelCaps::resolve(ProviderKind::DeepSeek, "deepseek-v4-pro", None).vision);
    // A generic OpenAI-compatible endpoint can front anything; vision is opt-in
    // per slot rather than guessed from an arbitrary id.
    assert!(!ModelCaps::resolve(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", None).vision);
    assert!(ModelCaps::resolve(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", Some(true)).vision);
    // OpenRouter is the same shape: the gateway fronts blind and sighted models
    // alike, so vision is the pinned model's property — opt-in per slot. The
    // built-in cast splits by role: its explorer default is multimodal and pins
    // vision on; its text-only synth default leaves the slot None. The literal id
    // below is arbitrary — the OpenRouter classifier ignores it, keying on the kind.
    assert!(
        !ModelCaps::resolve(
            ProviderKind::OpenRouter,
            "~anthropic/claude-sonnet-latest",
            None
        )
        .vision
    );
    assert!(
        ModelCaps::resolve(
            ProviderKind::OpenRouter,
            "~anthropic/claude-sonnet-latest",
            Some(true)
        )
        .vision
    );
    // The override pins in both directions.
    assert!(!ModelCaps::resolve(ProviderKind::Anthropic, "claude-sonnet-4-6", Some(false)).vision);

    // The transport channel is the *other* half of the see-∧-transport predicate, and
    // it's a property of the wire protocol alone — never overridden. Anthropic/Gemini
    // carry an image inside a tool result; the openai wire forbids it, so a seeing
    // openai VLM must get the image on a user turn (the break-rewrite-resume path).
    assert!(
        ModelCaps::resolve(ProviderKind::Anthropic, "claude-sonnet-4-6", None).tool_result_images
    );
    assert!(ModelCaps::resolve(ProviderKind::Gemini, "gemini-3.5-flash", None).tool_result_images);
    assert!(
        !ModelCaps::resolve(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", Some(true))
            .tool_result_images,
        "an openai VLM sees, but its transport can't carry a tool-result image"
    );
    // The vision override never flips the transport channel — it's the wire's property.
    assert!(!ModelCaps::resolve(ProviderKind::Openai, "anything", Some(true)).tool_result_images);
    // OpenRouter's transport is *worse* than a 400: rig's converter silently rewrites
    // a tool-result image to placeholder text, so the channel must stay closed — a
    // seeing OpenRouter model gets its image on the user turn instead.
    assert!(
        !ModelCaps::resolve(
            ProviderKind::OpenRouter,
            "~google/gemini-flash-latest",
            Some(true)
        )
        .tool_result_images,
        "an OpenRouter VLM sees, but rig's tool-result converter would drop the bytes"
    );
}

#[test]
fn builtin_openai_cast_is_cheap_gemma_explorer_strong_gemma_synth() {
    // The chosen mapping for the local default endpoint: small E4B drives the
    // tool-heavy exploration, the larger 26B writes the answer — the cheap-
    // explorer/strong-synth pattern, local edition.
    let (explorer, synth) = default_models(ProviderKind::Openai);
    assert_eq!(explorer, "Gemma-4-E4B-it-GGUF");
    assert_eq!(synth, "Gemma-4-26B-A4B-it-GGUF");

    // And the built-in `openai` cast resolves to exactly those, on the openai backend.
    let cfg = Config::builtin();
    let cast = cfg.resolve_cast("openai-local").expect("built-in openai cast");
    let e = cast
        .require_slot(ModelRole::Explorer)
        .expect("explorer slot");
    let s = cast.require_slot(ModelRole::Synth).expect("synth slot");
    assert_eq!(e.id, explorer);
    assert_eq!(s.id, synth);
    let backend = cfg.resolve_backend(&s.backend).expect("slot backend");
    assert_eq!(backend.kind, ProviderKind::Openai);
}

// ---- a minimal scripted client ----------------------------------------------
//
// The offline harness in `src/test_support.rs` is `#[cfg(test)]` — compiled into
// the lib's own unit tests only, never into the library this integration binary
// links (deliberate: it must not ship). So this file carries its own minimal
// scripted client and injects it through the same public seam the server uses for
// live clients: `Arm::new`. Same discipline as the lib harness: content-driven
// responders that branch on the inbound request, not consumption-ordered queues.

type Responder = Arc<
    dyn Fn(
            &rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::completion::CompletionResponse<()>,
            rig_core::completion::CompletionError,
        > + Send
        + Sync,
>;

/// A snapshot of one inbound request: which model the loop addressed and the
/// request shape that rode along — enough to prove per-arm routing and shaping.
#[derive(Debug, Clone)]
struct Seen {
    preamble: Option<String>,
    max_tokens: Option<u64>,
    additional_params: Option<Value>,
}

/// A scripted client that serves exactly ONE model. If the consult loop ever
/// routes another model id here, `completion` panics — the teeth of the
/// mixed-cast routing test: two of these stand in for two distinct backends.
#[derive(Clone)]
struct ScriptedClient {
    expect_model: String,
    responder: Responder,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl ScriptedClient {
    fn new<F>(expect_model: &str, responder: F) -> Self
    where
        F: Fn(
                &rig_core::completion::CompletionRequest,
            ) -> Result<
                rig_core::completion::CompletionResponse<()>,
                rig_core::completion::CompletionError,
            > + Send
            + Sync
            + 'static,
    {
        Self {
            expect_model: expect_model.to_string(),
            responder: Arc::new(responder),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request this client served, in call order.
    fn seen(&self) -> Vec<Seen> {
        self.log.lock().expect("scripted log poisoned").clone()
    }
}

#[derive(Clone)]
struct ScriptedModel {
    id: String,
    client: ScriptedClient,
}

/// Streaming placeholder: kaibo never streams; exists only for the trait bounds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct NoStream;

impl rig_core::completion::GetTokenUsage for NoStream {
    fn token_usage(&self) -> rig_core::completion::Usage {
        rig_core::completion::Usage::new()
    }
}

impl rig_core::client::CompletionClient for ScriptedClient {
    type CompletionModel = ScriptedModel;
}

impl rig_core::completion::CompletionModel for ScriptedModel {
    type Response = ();
    type StreamingResponse = NoStream;
    type Client = ScriptedClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            id: model.into(),
            client: client.clone(),
        }
    }

    async fn completion(
        &self,
        request: rig_core::completion::CompletionRequest,
    ) -> Result<rig_core::completion::CompletionResponse<()>, rig_core::completion::CompletionError>
    {
        assert_eq!(
            self.id, self.client.expect_model,
            "this client serves only {:?} — the loop routed a phase to the wrong client",
            self.client.expect_model
        );
        self.client
            .log
            .lock()
            .expect("scripted log poisoned")
            .push(Seen {
                preamble: request.preamble.clone().or_else(|| {
                    request.chat_history.iter().find_map(|m| match m {
                        rig_core::completion::message::Message::System { content } => {
                            Some(content.clone())
                        }
                        _ => None,
                    })
                }),
                max_tokens: request.max_tokens,
                additional_params: request.additional_params.clone(),
            });
        (self.client.responder)(&request)
    }

    async fn stream(
        &self,
        _request: rig_core::completion::CompletionRequest,
    ) -> Result<
        rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
        rig_core::completion::CompletionError,
    > {
        unimplemented!("kaibo drives the non-streaming prompt loop; the mock never streams")
    }
}

/// A final text answer — ends the tool loop.
fn text_response(text: impl Into<String>) -> rig_core::completion::CompletionResponse<()> {
    rig_core::completion::CompletionResponse {
        choice: rig_core::OneOrMany::one(rig_core::completion::message::AssistantContent::text(
            text,
        )),
        usage: rig_core::completion::Usage::new(),
        raw_response: (),
        message_id: None,
    }
}

/// A single tool call — drives one more loop turn.
fn tool_call_response(
    id: &str,
    name: &str,
    args: Value,
) -> rig_core::completion::CompletionResponse<()> {
    rig_core::completion::CompletionResponse {
        choice: rig_core::OneOrMany::one(
            rig_core::completion::message::AssistantContent::tool_call(id, name, args),
        ),
        usage: rig_core::completion::Usage::new(),
        raw_response: (),
        message_id: None,
    }
}

/// Several tool calls in ONE assistant turn — the co-tool-call case (`view_image`
/// alongside `run_kaish`). rig runs them together and folds both results into a single
/// user turn, exactly the shape the view_image turn-boundary break must tolerate.
fn tool_calls_response(
    calls: &[(&str, &str, Value)],
) -> rig_core::completion::CompletionResponse<()> {
    use rig_core::completion::message::AssistantContent;
    let contents: Vec<AssistantContent> = calls
        .iter()
        .map(|(id, name, args)| AssistantContent::tool_call(*id, *name, args.clone()))
        .collect();
    rig_core::completion::CompletionResponse {
        choice: rig_core::OneOrMany::many(contents).expect("at least one tool call"),
        usage: rig_core::completion::Usage::new(),
        raw_response: (),
        message_id: None,
    }
}

/// True if the request declares a tool named `name`.
fn has_tool(req: &rig_core::completion::CompletionRequest, name: &str) -> bool {
    req.tools.iter().any(|t| t.name == name)
}

/// Everything the model was shown in user turns — `User` text *and* tool-result
/// text — oldest→newest, so a responder can branch on a report's arrival.
fn transcript_text(req: &rig_core::completion::CompletionRequest) -> String {
    use rig_core::completion::message::{Message, ToolResultContent, UserContent};
    req.chat_history
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.clone()),
                        UserContent::ToolResult(tr) => Some(
                            tr.content
                                .iter()
                                .filter_map(|rc| match rc {
                                    ToolResultContent::Text(t) => Some(t.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The mixed-cast proof, offline: a consult whose explorer arm and synth arm ride
/// two *different* clients — two backends in miniature, each refusing to serve any
/// model but its own — through the public `consult` seam. The driver delegates a
/// sweep to `explore′` (which must land on the explorer's client), the report
/// crosses back, and the answer concludes on the synth's client. Each phase also
/// carries ITS arm's request shape (`max_tokens`, `additional_params`), per-arm by
/// construction — the chimera promise of docs/casts.md, proven with no network.
#[tokio::test]
async fn mixed_cast_consult_routes_each_phase_to_its_own_client() {
    const EXPLORER_MODEL: &str = "deepseek-v4-flash";
    const SYNTH_MODEL: &str = "claude-sonnet-4-6";
    const REPORT: &str = "MIXED_REPORT: src/foo.rs:1 fn target_marker";

    let explorer_client = ScriptedClient::new(EXPLORER_MODEL, |req| {
        // The delegated sweep: run_kaish only, no nested explore′.
        assert!(has_tool(req, "run_kaish"), "explorer gets run_kaish");
        assert!(!has_tool(req, "explore"), "explorer must not nest explore′");
        Ok(text_response(REPORT))
    });
    let synth_client = ScriptedClient::new(SYNTH_MODEL, |req| {
        assert!(
            has_tool(req, "run_kaish") && has_tool(req, "explore"),
            "the driver gets both tools"
        );
        if transcript_text(req).contains("MIXED_REPORT") {
            // The cross-client report came back → answer.
            Ok(text_response("ANSWER: target_marker at src/foo.rs:1"))
        } else {
            // Delegate a sweep — this must run on the *other* client.
            Ok(tool_call_response(
                "t-explore",
                "explore",
                json!({ "question": "find target_marker" }),
            ))
        }
    });

    // Each arm carries its own request shape, as `Arm::from_slot` would fit per slot
    // (a DeepSeek-shaped explorer, an adaptive-Anthropic synth).
    let explorer_params = json!({ "thinking": { "type": "enabled" }, "reasoning_effort": "high" });
    let synth_params = json!({ "thinking": { "type": "adaptive" } });
    let explorer = Arm::new(
        explorer_client.clone(),
        EXPLORER_MODEL,
        8192,
        Some(explorer_params.clone()),
        ModelCaps {
            vision: false,
            tool_result_images: true,
        },
    );
    let synth = Arm::new(
        synth_client.clone(),
        SYNTH_MODEL,
        16384,
        Some(synth_params.clone()),
        ModelCaps {
            vision: true,
            tool_result_images: true,
        },
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/foo.rs"), "fn target_marker() {}\n").unwrap();

    let out = consult(
        "Where is target_marker defined?",
        None,
        dir.path(),
        &explorer,
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("mixed-cast scripted consult should succeed");

    assert!(
        out.answer.contains("src/foo.rs:1"),
        "the synth's final text is the answer, got: {:?}",
        out.answer
    );
    assert!(
        out.report.contains("MIXED_REPORT"),
        "the cross-client sweep's report aggregated, got: {:?}",
        out.report
    );

    // Routing teeth: both clients actually served (each one panics inside
    // `completion` on any other model id, so non-empty logs + a green run mean
    // every phase landed on exactly its own client).
    let e = explorer_client.seen();
    let s = synth_client.seen();
    assert!(!e.is_empty(), "the explorer client was driven");
    assert!(!s.is_empty(), "the synth client was driven");

    // Per-arm request shaping: each phase carried ITS arm's params and headroom.
    for r in &e {
        assert_eq!(
            r.additional_params.as_ref(),
            Some(&explorer_params),
            "explorer requests carry the explorer arm's params"
        );
        assert_eq!(r.max_tokens, Some(8192), "explorer headroom is its arm's");
    }
    for r in &s {
        assert_eq!(
            r.additional_params.as_ref(),
            Some(&synth_params),
            "synth requests carry the synth arm's params"
        );
        assert_eq!(r.max_tokens, Some(16384), "synth headroom is its arm's");
    }

    // Role framing crossed with the routing: the explorer's client saw the report
    // preamble, the synth's client the consult-driver preamble.
    assert!(
        e[0].preamble.as_deref().unwrap_or("").contains("explorer"),
        "explorer client got the report preamble: {:?}",
        e[0].preamble
    );
    assert!(
        s[0].preamble
            .as_deref()
            .unwrap_or("")
            .contains("second tool, `explore`"),
        "synth client got the consult preamble: {:?}",
        s[0].preamble
    );
}

/// True if any tool result in the request history carried an *image* part. The
/// text-only `transcript_text` can't see this, and that's the point: an image part
/// is exactly what a base64 tool envelope must become to reach model context.
fn request_has_tool_result_image(req: &rig_core::completion::CompletionRequest) -> bool {
    use rig_core::completion::message::{Message, ToolResultContent, UserContent};
    req.chat_history.iter().any(|m| match m {
        Message::User { content } => content.iter().any(|c| {
            matches!(
                c,
                UserContent::ToolResult(tr)
                    if tr.content.iter().any(|rc| matches!(rc, ToolResultContent::Image(_)))
            )
        }),
        _ => false,
    })
}

/// True if any user message carries an *image part* (not a tool-result image). This
/// is exactly what a base64 envelope must become on a transport that can't carry an
/// image in a tool result — the openai user-turn channel. The recorder (`Seen`)
/// captures only text, so the assertion has to walk `req.chat_history` here.
fn request_has_user_image(req: &rig_core::completion::CompletionRequest) -> bool {
    use rig_core::completion::message::{Message, UserContent};
    req.chat_history.iter().any(|m| match m {
        Message::User { content } => content.iter().any(|c| matches!(c, UserContent::Image(_))),
        _ => false,
    })
}

/// vision-in, end to end and offline. A vision-capable synth is offered `view_image`,
/// calls it on a real file in the tree, and the bytes return as a rig *image part* in
/// the next turn — not text. The synth can only reach its answer once it has seen the
/// image, so a green run proves the whole chain: caps → toolset assembly → VFS read
/// through the read-only kernel → a typed `ToolResultContent::Image` block → image in
/// model context. The proof of the "all phases" decision, exercised on the `consult`
/// driver (the synth arm) — where `view_image` rides now.
#[tokio::test]
async fn a_vision_synth_sees_an_image_through_view_image() {
    const SYNTH_MODEL: &str = "claude-sonnet-4-6";

    // A real PNG (signature + filler — we never decode it) inside the workspace.
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(b"kaibo-pixels");
    std::fs::write(root.join("diagram.png"), &png).unwrap();

    let synth_client = ScriptedClient::new(SYNTH_MODEL, |req| {
        if request_has_tool_result_image(req) {
            // The image landed in context as an image part — only now answer.
            Ok(text_response("I can see the diagram. DONE"))
        } else {
            assert!(
                has_tool(req, "view_image"),
                "a vision synth must be offered view_image"
            );
            Ok(tool_call_response(
                "t-view",
                "view_image",
                json!({ "path": "diagram.png" }),
            ))
        }
    });
    let synth = Arm::new(
        synth_client.clone(),
        SYNTH_MODEL,
        16384,
        None,
        ModelCaps {
            vision: true,
            tool_result_images: true,
        },
    );

    let answer = consult(
        "What does the diagram show?",
        None,
        &root,
        &unused_explorer(&synth_client),
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("vision consult should succeed")
    .answer;

    // The only route to "DONE" runs through the image-present branch, so this single
    // assertion proves the round-trip: the model actually received the image part.
    assert!(
        answer.contains("DONE"),
        "synth answered only after seeing the image part: {answer:?}"
    );
}

/// The negative: a blind synth (the default `ModelCaps { vision: false, tool_result_images: true }`) is never
/// offered `view_image`. Pin it on the same `consult` driver so the gate can't
/// silently flip open for a text-only model.
#[tokio::test]
async fn a_blind_synth_is_not_offered_view_image() {
    const SYNTH_MODEL: &str = "deepseek-v4-pro";

    let dir = tempfile::tempdir().unwrap();
    let synth_client = ScriptedClient::new(SYNTH_MODEL, |req| {
        assert!(
            !has_tool(req, "view_image"),
            "a blind synth must NOT see view_image"
        );
        assert!(has_tool(req, "run_kaish"), "but it still drives run_kaish");
        Ok(text_response("text-only answer"))
    });
    let synth = Arm::new(
        synth_client.clone(),
        SYNTH_MODEL,
        16384,
        None,
        ModelCaps {
            vision: false,
            tool_result_images: true,
        },
    );

    consult(
        "anything",
        None,
        dir.path(),
        &unused_explorer(&synth_client),
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("blind consult should succeed");
    assert!(
        !synth_client.seen().is_empty(),
        "the synth client was driven"
    );
}

/// The openai VLM path, offline: a vision synth on a transport that can't carry an
/// image in a tool result (`tool_result_images: false`) still *sees* the image — it
/// arrives on the **user-turn** channel. The synth calls `view_image`; the loop breaks
/// at the turn boundary, rewrites the result onto a separate user `Image` turn, and
/// resumes. The responder answers only once it sees a user image, and asserts no
/// tool-result image copy survives — the rewrite *moved* the bytes, it didn't duplicate
/// them. This is the core regression for the whole break-rewrite-resume path.
///
/// Necessary but NOT sufficient: the mock returns its scripted answer regardless of
/// wire validity, so a rewrite that orphaned a `tool_use` would still pass here. Only
/// the live VLM probe catches that — see `docs/oai-images.md`.
#[tokio::test]
async fn an_openai_vlm_sees_an_image_on_the_user_turn_channel() {
    const SYNTH_MODEL: &str = "qwen2-vl-local";

    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(b"kaibo-pixels");
    std::fs::write(root.join("diagram.png"), &png).unwrap();

    let synth_client = ScriptedClient::new(SYNTH_MODEL, |req| {
        if request_has_user_image(req) {
            // The image arrived on the user channel — and the tool-result copy is gone.
            assert!(
                !request_has_tool_result_image(req),
                "the rewrite must MOVE the image to a user turn, not leave a tool-result copy"
            );
            Ok(text_response("I can see the diagram. DONE"))
        } else {
            assert!(
                has_tool(req, "view_image"),
                "an openai vision synth must still be offered view_image"
            );
            Ok(tool_call_response(
                "t-view",
                "view_image",
                json!({ "path": "diagram.png" }),
            ))
        }
    });
    // vision: true but tool_result_images: false — the openai transport. This flips
    // run_phase onto the break-rewrite-resume path.
    let synth = Arm::new(
        synth_client.clone(),
        SYNTH_MODEL,
        16384,
        None,
        ModelCaps {
            vision: true,
            tool_result_images: false,
        },
    );

    let answer = consult(
        "What does the diagram show?",
        None,
        &root,
        &unused_explorer(&synth_client),
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("openai-VLM consult should resume after the view_image break")
    .answer;

    assert!(
        answer.contains("DONE"),
        "the synth answered only after the image arrived on the user turn: {answer:?}"
    );
    // The loop broke and resumed: a view_image turn, then a separate resumed turn.
    assert!(
        synth_client.seen().len() >= 2,
        "the loop must have broken on view_image and resumed: {} request(s)",
        synth_client.seen().len()
    );
}

/// Co-tool-call on the openai path: one assistant turn calls `view_image` AND
/// `run_kaish` together. The break must wait for the turn boundary so *both* tools run
/// and both results land before the rewrite — breaking mid-`view_image` would orphan
/// the `run_kaish` `tool_use`. A clean resume to an answer is the offline proof the
/// transcript stayed well-formed across the co-tool-call turn.
#[tokio::test]
async fn an_openai_vlm_co_tool_call_view_image_and_run_kaish_resumes_cleanly() {
    const SYNTH_MODEL: &str = "qwen2-vl-local";

    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(b"kaibo-pixels");
    std::fs::write(root.join("diagram.png"), &png).unwrap();

    let synth_client = ScriptedClient::new(SYNTH_MODEL, |req| {
        if request_has_user_image(req) {
            assert!(
                !request_has_tool_result_image(req),
                "the view_image image moved to a user turn; no tool-result copy remains"
            );
            Ok(text_response("DONE"))
        } else {
            // One turn, two tools — view_image alongside run_kaish.
            Ok(tool_calls_response(&[
                ("t-view", "view_image", json!({ "path": "diagram.png" })),
                ("t-ls", "run_kaish", json!({ "script": "ls" })),
            ]))
        }
    });
    let synth = Arm::new(
        synth_client.clone(),
        SYNTH_MODEL,
        16384,
        None,
        ModelCaps {
            vision: true,
            tool_result_images: false,
        },
    );

    let answer = consult(
        "What does the diagram show?",
        None,
        &root,
        &unused_explorer(&synth_client),
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("a co-tool-call view_image turn must resume without orphaning run_kaish")
    .answer;

    assert!(
        answer.contains("DONE"),
        "the synth resumed and answered after the co-tool-call turn: {answer:?}"
    );
}

// Validation of the user-config path: load the real user config
// (~/.config/kaibo/config.toml), resolve a *non-built-in* cast, and run its synth
// slot against Lemonade. Proves config → cast → backend → arm wiring for a second
// model on the same backend — the headline feature, exercised live.
#[tokio::test]
#[ignore = "loads ~/.config/kaibo/config.toml and hits local Lemonade (GLM); run with --ignored while it's up"]
async fn secondary_local_cast_from_user_config_runs() {
    // Part 1 — the user config file parses and its extra casts resolve to the
    // right model + endpoint (no network).
    let cfg = Config::load(None).expect("load user config from the XDG default path");
    let glm = cfg
        .resolve_cast("glm")
        .expect("the user config should define a `glm` cast");
    let glm_synth = glm
        .require_slot(ModelRole::Synth)
        .expect("the glm cast has a synth slot");
    assert_eq!(glm_synth.id, "GLM-4.5-Air-UD-Q4K-XL-GGUF");
    let backend = cfg
        .resolve_backend(&glm_synth.backend)
        .expect("the glm synth slot's backend resolves");
    assert_eq!(backend.resolved_base_url(), "http://localhost:13305/api/v1");
    assert_eq!(backend.kind, ProviderKind::Openai);
    let qwen = cfg.resolve_cast("qwen").expect("a `qwen` cast");
    assert_eq!(
        qwen.require_slot(ModelRole::Synth).expect("qwen synth").id,
        "Qwen3-Coder-Next-GGUF"
    );

    // Part 2 — run a *second* model on that backend live to prove the resolve →
    // client → call path end-to-end. The GLM/Qwen builds don't always load on
    // Lemonade; Gemma does, so swap the id within the slot's backend (exactly what
    // a per-call bare model override does — pins drop, the new id classifies fresh).
    let secondary = ModelSlot::bare(glm_synth.backend.clone(), "Gemma-4-E4B-it-GGUF");
    let arm = Arm::from_slot(backend, &secondary, ModelRole::Synth, &cfg.defaults)
        .expect("secondary arm builds");
    let (answer, _usage) = oneshot(
        "Context: src/sandbox.rs builds a read-only kernel; writes and external \
         commands are refused.\n\nIn one sentence, what does kaibo's read-only sandbox \
         prevent?",
        &[],
        &arm,
        &PhaseContext::default(),
    )
    .await
    .expect("secondary-cast oneshot against Lemonade should succeed");
    eprintln!("=== SECONDARY CAST ANSWER ===\n{answer}\n");
    assert!(!answer.trim().is_empty(), "expected a non-empty answer");
}

// The recomposed consult (one loop, {run_kaish, explore′}) on the weakest target —
// the §2.1 weak-model validation. Asserts a grounded answer; the aggregated report
// is non-empty iff the model chose to delegate to explore′, which we log but do NOT
// assert (Gemma may read directly — a fixed pipeline is more robust for weak models,
// per the panel; if delegation proves shaky here, that's a note, not a failure).
#[tokio::test]
#[ignore = "hits the local OpenAI-compatible (Gemma) server (consult, one loop); run with --ignored while it's up"]
async fn recomposed_consult_runs_against_local_gemma() {
    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        root,
        &builtin_arm("openai-local", ModelRole::Explorer),
        &builtin_arm("openai-local", ModelRole::Synth),
        &cfg,
        None,
    )
    .await
    .expect("consult against local gemma should succeed");

    eprintln!(
        "=== explore′ delegated {} time(s); aggregated report ===\n{}\n",
        out.report.matches("---").count() + if out.report.is_empty() { 0 } else { 1 },
        out.report
    );
    eprintln!("=== ANSWER ===\n{}\n", out.answer);

    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

// The `oneshot` seam live: a thin, toolless answer grounded only in the context the
// caller pasted into the prompt (no codebase access, no run_kaish). The counterpart
// to consult — consult-without-context's "investigate the real tree" path is covered
// by `recomposed_consult_runs_against_local_gemma` above.
#[tokio::test]
#[ignore = "hits the local OpenAI-compatible (Gemma) server (oneshot); run with --ignored while it's up"]
async fn oneshot_answers_from_pasted_context() {
    let arm = builtin_arm("openai-local", ModelRole::Synth);

    let (answer, _usage) = oneshot(
        "Context: src/sandbox.rs builds a read-only kernel; the LocalFs read-only \
         mount refuses every write.\n\nIn one sentence, which file enforces kaibo's \
         read-only sandbox?",
        &[],
        &arm,
        &PhaseContext::default(),
    )
    .await
    .expect("oneshot against local gemma should succeed");
    eprintln!("=== ONESHOT ===\n{answer}\n");
    assert!(
        answer.to_lowercase().contains("sandbox"),
        "should answer about the sandbox file from the pasted context, got: {answer}"
    );
}

// Live thinking-on checks for the keyed providers. They exercise the risky paths:
// Anthropic's thinking blocks round-tripping through the tool loop, and Gemini's
// thinkingConfig shape (the thinkingLevel the whole 3-line takes).
#[tokio::test]
#[ignore = "hits the DeepSeek API (consult); run with --ignored and a key"]
async fn two_phase_consult_via_deepseek() {
    if let Err(e) = load(ProviderKind::DeepSeek) {
        panic!("no DeepSeek credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        env!("CARGO_MANIFEST_DIR"),
        &builtin_arm("deepseek", ModelRole::Explorer),
        &builtin_arm("deepseek", ModelRole::Synth),
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("deepseek consult should succeed");
    eprintln!("=== DEEPSEEK ANSWER ===\n{}\n", out.answer);
    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

#[tokio::test]
#[ignore = "hits the Gemini API (consult); run with --ignored and a key"]
async fn two_phase_consult_via_gemini() {
    if let Err(e) = load(ProviderKind::Gemini) {
        panic!("no Gemini credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        env!("CARGO_MANIFEST_DIR"),
        &builtin_arm("gemini", ModelRole::Explorer),
        &builtin_arm("gemini", ModelRole::Synth),
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("gemini consult should succeed");
    eprintln!("=== GEMINI ANSWER ===\n{}\n", out.answer);
    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

#[tokio::test]
#[ignore = "hits the Anthropic API (consult); run with --ignored and a key"]
async fn two_phase_consult_answers_from_the_real_tree() {
    // Surface a clear message if the credential is missing, before the API call.
    if let Err(e) = load(ProviderKind::Anthropic) {
        panic!("no Anthropic credential for live test: {e}");
    }

    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        root,
        &builtin_arm("anthropic", ModelRole::Explorer),
        &builtin_arm("anthropic", ModelRole::Synth),
        &cfg,
        None,
    )
    .await
    .expect("consult should succeed");

    eprintln!("=== REPORT (explorer) ===\n{}\n", out.report);
    eprintln!("=== ANSWER (synth) ===\n{}\n", out.answer);

    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

#[tokio::test]
#[ignore = "hits the OpenRouter API (keyed gateway); run with --ignored and OPENROUTER_API_KEY"]
async fn openrouter_consult_round_trips() {
    // The live proof of the OpenRouter arm: the unified `{reasoning:{effort}}` param,
    // the `max_completion_tokens` workaround (rig drops native max_tokens for this
    // provider — without the injection a thinking-on answer would starve), and the
    // `with_app_identity` headers all have to be accepted end-to-end. Runs the built-in
    // openrouter cast (the `~author/family-latest` aliases) so a regression in any of
    // those surfaces as a live failure here, not in production.
    if let Err(e) = load(ProviderKind::OpenRouter) {
        panic!("no OpenRouter credential for live test: {e}");
    }

    let cfg = Config::builtin();
    let backend = cfg
        .resolve_backend("openrouter")
        .expect("openrouter backend");
    let explorer_slot = cfg
        .resolve_cast("openrouter")
        .unwrap()
        .require_slot(ModelRole::Explorer)
        .unwrap()
        .clone();
    let synth_slot = cfg
        .resolve_cast("openrouter")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .clone();
    let explorer = Arm::from_slot(backend, &explorer_slot, ModelRole::Explorer, &cfg.defaults)
        .expect("explorer arm on openrouter");
    let synth = Arm::from_slot(backend, &synth_slot, ModelRole::Synth, &cfg.defaults)
        .expect("synth arm on openrouter");

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        env!("CARGO_MANIFEST_DIR"),
        &explorer,
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("openrouter consult should succeed (reasoning + max_completion_tokens accepted)");

    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

/// One minimal OpenRouter completion carrying kaibo's shaped params, with the
/// gateway's usage accounting on. Returns the parsed response JSON, and refuses a
/// rejection — this is the "does kaibo's shape work" probe, so a 4xx is a failure.
///
/// Delegates to [`openrouter_probe_raw`] rather than building its own request: the
/// ladder probes below need the same body with a *different* prompt and the ability to
/// read a 400 body, and two copies of one request shape is how a probe and the thing it
/// probes drift apart.
#[cfg(test)]
async fn openrouter_probe_completion(key: &str, model: &str, params: &Value) -> Value {
    openrouter_probe_completion_with(
        key,
        model,
        params,
        "How many prime numbers are there between 10 and 50? \
         Work it out carefully, then answer with just the count.",
    )
    .await
}

#[tokio::test]
#[ignore = "hits the OpenRouter API (keyed gateway); run with --ignored and OPENROUTER_API_KEY"]
async fn openrouter_reasoning_accounting_live() {
    // "Reasoning on by default" is doctrine, so it gets *measured*, not assumed
    // (2026-07-03: a live consult averaged ~150 output tokens per turn — thin
    // enough to question whether effort was landing). This posts kaibo's exact
    // shaped params — `ModelShape::to_params`, the seam every OpenRouter arm
    // rides — with OpenRouter's usage accounting on, and reads what the gateway
    // actually billed: effort-on must show reasoning tokens, and the structural
    // disable (`effort = "none"` ⇒ `{"reasoning":{"enabled":false}}`) must not.
    // `live_key`, not `credentials::load`: an operator's key file needn't sit at the
    // built-in default path, and here it doesn't — the config points at
    // `~/.openrouter-key.txt` while `load` looks for `~/.openrouter-key`, so this probe
    // was unrunnable on the machine that owns the key. See `live_key`.
    let key = live_key(ProviderKind::OpenRouter);
    // The built-in cast's explorer alias — the slot the doctrine most needs to
    // hold on, since the explorer runs the most turns.
    let model = "~google/gemini-flash-latest";
    let shape = ModelShape::resolve(
        ProviderKind::OpenRouter,
        model,
        ThinkingStyleOverride::Auto,
    );

    // Default posture: effort = high ⇒ the gateway bills reasoning tokens. The
    // no-collection routing pin rides along exactly as every live arm sends it —
    // so this also proves deny-routing still reaches the built-in cast's models.
    let params = shape.to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT);
    let params = kaibo::consult::inject_provider_prefs(
        ProviderKind::OpenRouter,
        params,
        kaibo::config::DataCollection::Deny,
    )
    .expect("openrouter always sends params");
    let resp = openrouter_probe_completion(&key, model, &params).await;
    let reasoning = resp["usage"]["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0);
    eprintln!("=== effort={DEFAULT_EFFORT} usage: {}", resp["usage"]);
    assert!(
        reasoning > 0,
        "effort={DEFAULT_EFFORT} must bill reasoning tokens — thinking-on-by-default \
         isn't landing, got usage: {}",
        resp["usage"]
    );

    // The opt-out: the structural disable really turns reasoning off.
    let params = shape
        .to_params(THINKING_BUDGET, None, None, "none")
        .expect("openrouter always sends params");
    let resp = openrouter_probe_completion(&key, model, &params).await;
    let reasoning = resp["usage"]["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0);
    eprintln!("=== effort=none usage: {}", resp["usage"]);
    assert_eq!(
        reasoning, 0,
        "effort=none must not bill reasoning tokens, got usage: {}",
        resp["usage"]
    );
}

#[tokio::test]
#[ignore = "hits the Anthropic API on Opus 4.8 (adaptive-only tier); run with --ignored and a key"]
async fn adaptive_only_anthropic_model_round_trips() {
    // The real proof of the "fix anthropic" work: Opus 4.8 *rejects* the old
    // enabled/budget thinking block AND sampling outright (400). Pin both phases to it
    // (a bare slot on the anthropic backend — exactly what a per-call model override
    // builds) so a regression in the adaptive shape (or the output_config.effort
    // flatten through rig) surfaces as a live 400 here, not in production — what the
    // offline tests can't prove.
    if let Err(e) = load(ProviderKind::Anthropic) {
        panic!("no Anthropic credential for live test: {e}");
    }

    let cfg = Config::builtin();
    let backend = cfg.resolve_backend("anthropic").expect("anthropic backend");
    let slot = ModelSlot::bare("anthropic", "claude-opus-4-8");
    let explorer = Arm::from_slot(backend, &slot, ModelRole::Explorer, &cfg.defaults)
        .expect("explorer arm on opus 4.8");
    let synth = Arm::from_slot(backend, &slot, ModelRole::Synth, &cfg.defaults)
        .expect("synth arm on opus 4.8");

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        None,
        env!("CARGO_MANIFEST_DIR"),
        &explorer,
        &synth,
        &ConsultConfig::default(),
        None,
    )
    .await
    .expect("adaptive-only Opus 4.8 consult should succeed (no 400 on the thinking shape)");

    let lower = out.answer.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("read-only") || lower.contains("read only"),
        "answer should explain the read-only sandbox mechanism, got: {}",
        out.answer
    );
}

// ===========================================================================
// Live effort-ladder probes — what each PROVIDER really accepts
// ===========================================================================
//
// The other half of `tests/effort_wire.rs`. That suite is offline and asks rig:
// what will the *client* let kaibo send? These are live and ask the provider:
// what does the endpoint actually do with it? Both are needed, because rig's
// ceiling and the provider's ceiling are different numbers on two wires, and
// kaibo's docs used to assert one universal `none|low|…|max` ladder for
// everyone — wrong in every direction at once. Gemini is narrower (four rungs,
// and narrower still per model), OpenAI's ceiling AND floor are per-model, and
// DeepSeek's `none` didn't turn reasoning off the way kaibo was sending it.
//
// Every assertion here was observed on the wire on 2026-08-01 by raw HTTP
// against each provider's real endpoint — deliberately NOT through rig, whose
// typed enums reject before the wire and would tell us nothing about the
// provider. Each doc comment carries its dated measurement, so a later reader
// can tell what was measured from what was assumed.
//
// They follow the `openrouter_reasoning_accounting_live` precedent above: post
// raw, read the provider's own accounting back, and assert on the response
// BODY, not just the status. A rung that is accepted-but-ignored looks exactly
// like one that works unless you check whether reasoning actually happened.
//
// All `#[ignore]`d: they cost money and need network + keys.

/// Rungs kaibo can put in a slot's `effort` — the widest set any provider takes,
/// which is what a probe must try in order to discover a narrower one.
const ALL_RUNGS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A short arithmetic prompt: cheap, and every reasoning model spends *some*
/// thinking on it, so "did reasoning happen" is a live signal rather than noise.
const CHEAP_PROMPT: &str = "What is 1487 times 233? Give the number.";

/// A prompt with enough depth that the ladder's rungs can actually separate.
/// Used only where we assert *gradation*, never where we assert acceptance.
const HARD_PROMPT: &str = "Prove or disprove: every integer n>1 that divides 2^n-2 \
     must be composite or equal to 2. Reason carefully, then answer in one line.";

/// The client every live probe here uses — kaibo's own rustls+ring stack, so a probe
/// exercises the shipped TLS boundary too.
fn probe_http() -> reqwest::Client {
    kaibo::tls::https_client(std::time::Duration::from_secs(180)).expect("https client")
}

/// The API key a live probe should use for `kind`, resolved **the way kaibo itself
/// resolves one**: the operator's config names the backend and where its key lives
/// (`api_key_env` / `api_key_file`), and only when no configured backend of that kind
/// exists do we fall back to the built-in `credentials::load` path.
///
/// Going through the config is not politeness, it's necessity in both directions. The
/// `openai` kind is `key_optional` — a local llama.cpp server needs no key — so
/// `credentials::load` refuses it outright and the hosted Platform key is reachable
/// *only* through the backend that names it. And an operator's key file needn't sit at
/// the built-in default path: on this machine OpenRouter's key is
/// `~/.openrouter-key.txt` via config, while `load` looks for `~/.openrouter-key`.
///
/// Panics with the reason when no key resolves — an `#[ignore]`d probe that silently
/// no-ops is worse than one that doesn't run.
fn live_key(kind: ProviderKind) -> String {
    if let Ok(cfg) = Config::load(None) {
        let configured = cfg.backends.values().find(|b| {
            b.kind == kind
                // For the openai kind, only OpenAI Platform itself can answer a
                // Platform probe; a local endpoint sharing the kind cannot.
                && (kind != ProviderKind::Openai || b.is_hosted_openai())
        });
        if let Some(b) = configured {
            match b.resolve_key() {
                Ok(k) if k != kind.placeholder_key() => return k,
                Ok(_) => {}
                Err(e) => eprintln!("=== backend {:?} key did not resolve: {e}", b.name),
            }
        }
    }
    load(kind).unwrap_or_else(|e| panic!("no {kind:?} credential for live test: {e}"))
}

// --- DeepSeek --------------------------------------------------------------

async fn deepseek_probe(key: &str, body: Value) -> (u16, Value) {
    let resp = probe_http()
        .post("https://api.deepseek.com/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("deepseek request");
    let status = resp.status().as_u16();
    (status, resp.json().await.expect("deepseek response json"))
}

fn deepseek_reasoning_tokens(j: &Value) -> u64 {
    j["usage"]["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0)
}

/// DeepSeek validates `reasoning_effort` strictly against a *seven*-rung ladder — it is
/// the one provider whose accepted set is exactly the universal ladder kaibo's docs once
/// claimed for everyone. Observed 2026-08-01 on `deepseek-v4-pro`: all seven HTTP 200,
/// and a garbage value 400s with the ladder spelled out in the error.
///
/// That error text is the real oracle: it means a future DeepSeek ladder change fails
/// this test loudly instead of drifting past `docs/config.md`'s per-provider table.
#[tokio::test]
#[ignore = "hits the DeepSeek API; run with --ignored and a key"]
async fn deepseek_effort_ladder_live() {
    let key = live_key(ProviderKind::DeepSeek);

    for rung in ALL_RUNGS {
        let (status, json) = deepseek_probe(
            &key,
            json!({
                "model": "deepseek-v4-pro",
                "max_tokens": 2048,
                "thinking": { "type": "enabled" },
                "reasoning_effort": rung,
                "messages": [{ "role": "user", "content": CHEAP_PROMPT }],
            }),
        )
        .await;
        assert_eq!(
            status, 200,
            "DeepSeek should accept effort={rung}, got {status}: {json}"
        );
    }

    // The oracle: an off-ladder value is refused, and the refusal *enumerates* the
    // ladder. If DeepSeek adds or drops a rung, this assertion is what notices.
    let (status, json) = deepseek_probe(
        &key,
        json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 16,
            "thinking": { "type": "enabled" },
            "reasoning_effort": "zzz",
            "messages": [{ "role": "user", "content": "hi" }],
        }),
    )
    .await;
    assert_eq!(status, 400, "an off-ladder effort must be refused: {json}");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    for rung in ALL_RUNGS {
        assert!(
            msg.contains(rung),
            "DeepSeek's rejection should still enumerate `{rung}` — the ladder moved: {msg}"
        );
    }
}

/// The provider behavior that forced kaibo's DeepSeek off-switch to be *structural*.
///
/// The off-switch on DeepSeek belongs to `thinking.type`, not to `reasoning_effort`.
/// Observed 2026-08-01 on `deepseek-v4-pro`, one variable at a time:
///
/// | body                                              | reasoning tokens |
/// |---------------------------------------------------|------------------|
/// | `reasoning_effort: "none"` alone                   | 0                |
/// | `thinking: {"type":"disabled"}` alone              | 0                |
/// | `thinking: {"type":"enabled"}` + `effort: "none"`  | **160–253**      |
///
/// That third row is what kaibo used to send for `effort = "none"`: the explicit enable
/// wins, so an operator's opt-out did nothing and they paid for the reasoning anyway —
/// the silent-fallback shape this project refuses. `ThinkingStyle::DeepSeekEffort` now
/// emits the structural disable instead (`src/consult/shaping.rs`), the way
/// `OpenRouterEffort` already did.
///
/// So this test does two jobs. It pins the *provider* precedence that justifies the fix
/// (if DeepSeek ever makes `effort` win, the fix is no longer load-bearing and the
/// `none` guidance should be revisited), and it proves kaibo's own shaped params — the
/// exact blob `ModelShape::to_params` builds — bill zero reasoning on the wire.
#[tokio::test]
#[ignore = "hits the DeepSeek API; run with --ignored and a key"]
async fn deepseek_effort_none_needs_the_structural_disable_live() {
    let key = live_key(ProviderKind::DeepSeek);
    let base = |extra: Value| {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 2048,
            "messages": [{ "role": "user", "content": CHEAP_PROMPT }],
        });
        for (k, v) in extra.as_object().expect("object") {
            body[k] = v.clone();
        }
        body
    };

    // Each lever alone: really off.
    let (_, bare) = deepseek_probe(&key, base(json!({ "reasoning_effort": "none" }))).await;
    assert_eq!(
        deepseek_reasoning_tokens(&bare),
        0,
        "effort=none alone should suppress reasoning, got usage: {}",
        bare["usage"]
    );
    let (_, disabled) =
        deepseek_probe(&key, base(json!({ "thinking": { "type": "disabled" } }))).await;
    assert_eq!(
        deepseek_reasoning_tokens(&disabled),
        0,
        "thinking.type=disabled should suppress reasoning, got usage: {}",
        disabled["usage"]
    );

    // The pairing kaibo used to send. `thinking: enabled` wins and reasoning bills.
    let (_, both) = deepseek_probe(
        &key,
        base(json!({ "thinking": { "type": "enabled" }, "reasoning_effort": "none" })),
    )
    .await;
    assert!(
        deepseek_reasoning_tokens(&both) > 0,
        "documented provider precedence: thinking=enabled overrides effort=none. If this \
         now reports 0, DeepSeek changed its precedence — the structural disable is no \
         longer load-bearing and the `effort = \"none\"` guidance should be revisited. \
         usage: {}",
        both["usage"]
    );

    // And what kaibo sends TODAY: the shaped params, on the wire, billing nothing.
    let shaped = ModelShape::resolve(
        ProviderKind::DeepSeek,
        "deepseek-v4-pro",
        ThinkingStyleOverride::Auto,
    )
    .to_params(THINKING_BUDGET, None, None, "none")
    .expect("deepseek always sends a thinking block");
    let (status, kaibo_shape) = deepseek_probe(&key, base(shaped.clone())).await;
    assert_eq!(
        status, 200,
        "kaibo's shaped params must be accepted: {kaibo_shape}"
    );
    assert_eq!(
        deepseek_reasoning_tokens(&kaibo_shape),
        0,
        "kaibo's `effort = \"none\"` must actually stop the billing it names. sent {shaped}, \
         got usage: {}",
        kaibo_shape["usage"]
    );
}

// --- Gemini ----------------------------------------------------------------

async fn gemini_probe(key: &str, model: &str, level: &str, prompt: &str) -> (u16, Value) {
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let resp = probe_http()
        .post(url)
        .header("x-goog-api-key", key)
        .json(&json!({
            "contents": [{ "parts": [{ "text": prompt }] }],
            "generationConfig": {
                "maxOutputTokens": 8192,
                "thinkingConfig": { "thinkingLevel": level, "includeThoughts": true },
            },
        }))
        .send()
        .await
        .expect("gemini request");
    let status = resp.status().as_u16();
    (status, resp.json().await.expect("gemini response json"))
}

/// rig's four-variant `ThinkingLevel` (`minimal|low|medium|high`) is a CORRECT model of
/// Google's API, not a stale cap like its OpenAI `ReasoningEffort` enum. Observed
/// 2026-08-01 on `gemini-3.5-flash`: `none`, `xhigh` and `max` all 400 with a protobuf
/// enum error naming `google.ai.generativelanguage.v1beta.ThinkingConfig.ThinkingLevel`.
/// A schema-level refusal, so no Gemini model can take those rungs.
///
/// Consequence kaibo's docs now carry: `effort = "none"` cannot turn reasoning off on
/// the gemini kind at all — `minimal` is Google's off-switch (proven below).
#[tokio::test]
#[ignore = "hits the Gemini API; run with --ignored and a key"]
async fn gemini_thinking_level_ladder_live() {
    let key = live_key(ProviderKind::Gemini);

    for level in ["minimal", "low", "medium", "high"] {
        let (status, json) = gemini_probe(&key, "gemini-3.5-flash", level, CHEAP_PROMPT).await;
        assert_eq!(
            status, 200,
            "gemini-3.5-flash should accept thinkingLevel={level}: {json}"
        );
    }

    for level in ["none", "xhigh", "max"] {
        let (status, json) = gemini_probe(&key, "gemini-3.5-flash", level, CHEAP_PROMPT).await;
        assert_eq!(
            status, 400,
            "Google's ThinkingLevel enum has no `{level}` rung — if this now succeeds, \
             Google widened the enum and rig's four-variant cap became a stale ceiling \
             kaibo must document: {json}"
        );
        let msg = json["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("thinking_level") || msg.contains("ThinkingLevel"),
            "expected a thinking_level enum rejection for `{level}`, got: {msg}"
        );
    }
}

/// The Gemini ladder is narrower still *per model*: `gemini-pro-latest` refuses
/// `minimal` with a distinct, human-worded error (not the protobuf enum error) — a
/// model-capability refusal layered on top of the schema enum. Observed 2026-08-01.
///
/// So a cast pinning a Pro model has a three-rung ladder and no way to run thinking-off
/// at all — which is why `docs/config.md` says Gemini's off-switch is model-dependent
/// rather than just naming `minimal`.
#[tokio::test]
#[ignore = "hits the Gemini API; run with --ignored and a key"]
async fn gemini_pro_rejects_minimal_live() {
    let key = live_key(ProviderKind::Gemini);
    let (status, json) = gemini_probe(&key, "gemini-pro-latest", "minimal", CHEAP_PROMPT).await;
    assert_eq!(
        status, 400,
        "gemini-pro-latest is expected to refuse thinkingLevel=minimal: {json}"
    );
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.to_lowercase().contains("minimal"),
        "expected a model-capability refusal naming MINIMAL, got: {msg}"
    );
}

/// Gemini's rungs are *graded*, not decorative: thinking tokens climb with the level,
/// and `minimal` is genuinely thinking-off (no `thoughtsTokenCount` at all). Observed
/// 2026-08-01 on gemini-3.5-flash with `HARD_PROMPT`: minimal=0, low=494, medium=1269,
/// high=1974.
///
/// This is the accepted-and-reasoned vs accepted-but-ignored discriminator for the
/// gemini kind, and it doubles as the evidence that `minimal` is the right off-switch to
/// point operators at.
#[tokio::test]
#[ignore = "hits the Gemini API; run with --ignored and a key"]
async fn gemini_thinking_level_is_graded_live() {
    let key = live_key(ProviderKind::Gemini);
    let thoughts = |j: &Value| {
        j["usageMetadata"]["thoughtsTokenCount"]
            .as_u64()
            .unwrap_or(0)
    };

    let mut seen = Vec::new();
    for level in ["minimal", "low", "medium", "high"] {
        let (status, json) = gemini_probe(&key, "gemini-3.5-flash", level, HARD_PROMPT).await;
        assert_eq!(status, 200, "thinkingLevel={level}: {json}");
        seen.push((level, thoughts(&json)));
    }

    assert_eq!(
        seen[0].1, 0,
        "thinkingLevel=minimal is Gemini's off-switch — it must spend no thinking \
         tokens, got {seen:?}"
    );
    assert!(
        seen[3].1 > seen[1].1,
        "high must think measurably deeper than low, else the level is accepted but \
         ignored: {seen:?}"
    );
}

// --- OpenAI Platform -------------------------------------------------------

async fn openai_probe(key: &str, model: &str, effort: &str) -> (u16, Value) {
    let resp = probe_http()
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(key)
        .json(&json!({
            "model": model,
            "input": CHEAP_PROMPT,
            "max_output_tokens": 2048,
            "reasoning": { "effort": effort },
        }))
        .send()
        .await
        .expect("openai request");
    let status = resp.status().as_u16();
    (status, resp.json().await.expect("openai response json"))
}

/// The OpenAI ceiling is a **model** property, not a family property — and the *floor*
/// moves too. Observed 2026-08-01 against `/v1/responses`:
///
/// | model     | rejects          | the model's own "Supported values"        |
/// |-----------|------------------|-------------------------------------------|
/// | gpt-5.6-* | (none of them)   | takes `xhigh` AND `max`                   |
/// | gpt-5.2   | `max`            | none, low, medium, high, xhigh            |
/// | gpt-5.1   | `xhigh`, `max`   | none, low, medium, high                   |
/// | gpt-5     | `xhigh`, `max`   | minimal, low, medium, high  (no `none`!)  |
///
/// This is the measurement behind kaibo keeping **no** effort allowlist: any table we
/// baked in would be wrong within a release, and it would be wrong per model id rather
/// than per family, which is not a shape a classifier handles gracefully.
///
/// It also pins a trap for whoever re-derives this: validation is **two-layer**. An
/// off-schema sentinel (`"zzz"`) returns the API's *superset* enum for every model,
/// gpt-5 included — so the sentinel trick cannot reveal a model's real ladder. Only
/// posting the actual rung surfaces the model-capability refusal.
#[tokio::test]
#[ignore = "hits the OpenAI Platform API; run with --ignored and a hosted-openai backend key"]
async fn openai_effort_ceiling_is_per_model_live() {
    let key = live_key(ProviderKind::Openai);

    // The 5.6 line reaches the top of the ladder, cheap tier included — `max` is not
    // reserved for the flagship.
    for model in ["gpt-5.6-sol", "gpt-5.6-luna"] {
        for effort in ["xhigh", "max"] {
            let (status, json) = openai_probe(&key, model, effort).await;
            assert_eq!(status, 200, "{model} should accept effort={effort}: {json}");
            assert_eq!(
                json["reasoning"]["effort"].as_str(),
                Some(effort),
                "{model} should echo the requested effort back: {json}"
            );
            assert!(
                json["usage"]["output_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0,
                "{model} at effort={effort} must actually bill reasoning tokens: {}",
                json["usage"]
            );
        }
    }

    // Older ids cap lower. These refusals are why kaibo cannot ship one ladder for the
    // whole `openai` kind — nor even for the `gpt-5*` family.
    for (model, rejected) in [
        ("gpt-5.2", vec!["max"]),
        ("gpt-5.1", vec!["xhigh", "max"]),
        ("gpt-5", vec!["xhigh", "max"]),
    ] {
        for effort in rejected {
            let (status, json) = openai_probe(&key, model, effort).await;
            assert_eq!(
                status, 400,
                "{model} is expected to refuse effort={effort}: {json}"
            );
            let msg = json["error"]["message"].as_str().unwrap_or_default();
            assert!(
                msg.contains("not supported with the") && msg.contains(model),
                "expected a model-capability refusal naming {model}, got: {msg}"
            );
        }
    }

    // The two-layer trap, pinned: an off-schema value reports the superset, so it must
    // not be mistaken for the model's ladder.
    let (status, json) = openai_probe(&key, "gpt-5", "zzz").await;
    assert_eq!(status, 400, "an off-schema effort must be refused: {json}");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("xhigh") && msg.contains("max"),
        "the schema-level rejection is expected to list the superset even for a model \
         that refuses those rungs — if this changed, the sentinel trick became usable \
         for ladder discovery: {msg}"
    );
}

// --- OpenRouter ------------------------------------------------------------

/// One OpenRouter completion with the gateway's usage accounting on, returning whatever
/// came back — including a rejection body. The ladder probes need both knobs the older
/// `openrouter_probe_completion` lacked: a caller-chosen prompt (so rungs can separate
/// on a hard question) and the ability to READ a 400 rather than panic on it, since the
/// gateway's rejection text enumerates its ladder and that text is the oracle.
#[cfg(test)]
async fn openrouter_probe_raw(key: &str, model: &str, params: &Value, prompt: &str) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_completion_tokens": 8192,
        "usage": { "include": true },
    });
    for (k, v) in params.as_object().expect("shaped params are an object") {
        body[k] = v.clone();
    }
    probe_http()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("openrouter request")
        .json()
        .await
        .expect("openrouter response json")
}

/// [`openrouter_probe_raw`] plus "and it must have worked" — the shape every probe that
/// asserts *acceptance* wants. OpenRouter reports errors in the body with a 200-ish
/// envelope as readily as a 4xx, so the check is on `error`, not the status.
#[cfg(test)]
async fn openrouter_probe_completion_with(
    key: &str,
    model: &str,
    params: &Value,
    prompt: &str,
) -> Value {
    let json = openrouter_probe_raw(key, model, params, prompt).await;
    assert!(
        json.get("error").is_none(),
        "openrouter rejected the probe params: {json}"
    );
    json
}

/// OpenRouter really does expose the full seven-rung ladder, and `xhigh`/`max` are not
/// decoration: reasoning tokens climb with the rung. Observed 2026-08-01 on
/// `HARD_PROMPT` — `z-ai/glm-5.2` low=478, high=389, xhigh=809, max=1177;
/// `~openai/gpt-latest` low=87, high=115, xhigh=127, max=142.
///
/// (Note low>high on GLM in that run: sampling noise at adjacent rungs is real, which is
/// why the assertion below compares `max` against `high` — two rungs apart — rather than
/// demanding a monotonic staircase a live probe can't promise.)
///
/// This is the probe behind the claim in `src/consult/shaping.rs` and `docs/config.md`
/// that the openrouter kind reaches rungs the other kinds don't.
#[tokio::test]
#[ignore = "hits the OpenRouter API; run with --ignored and an OpenRouter key"]
async fn openrouter_top_rungs_reach_deeper_live() {
    let key = live_key(ProviderKind::OpenRouter);
    let model = "z-ai/glm-5.2";

    let mut seen = Vec::new();
    for effort in ["high", "xhigh", "max"] {
        let params = json!({ "reasoning": { "effort": effort }, "usage": { "include": true } });
        let resp = openrouter_probe_completion_with(&key, model, &params, HARD_PROMPT).await;
        let reasoning = resp["usage"]["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0);
        assert!(
            reasoning > 0,
            "effort={effort} must bill reasoning tokens: {}",
            resp["usage"]
        );
        seen.push((effort, reasoning));
    }

    assert!(
        seen[2].1 > seen[0].1,
        "`max` must reach deeper than `high`, else the extra rungs are accepted but \
         ignored and the docs should stop advertising them: {seen:?}"
    );

    // And the ladder is validated, not free-form — the rejection enumerates it.
    let params = json!({ "reasoning": { "effort": "zzz" } });
    let resp = openrouter_probe_raw(&key, model, &params, "hi").await;
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    for rung in ALL_RUNGS {
        assert!(
            msg.contains(rung),
            "OpenRouter's rejection should enumerate `{rung}` — the gateway ladder \
             moved: {msg}"
        );
    }
}

/// The gateway asymmetry, settled: OpenRouter carries `xhigh` and `max` to upstream
/// OpenAI models whose **direct** Platform API refuses those rungs. Observed 2026-08-01
/// — `openai/gpt-5.1` and `openai/gpt-5` both succeed with reasoning tokens billed
/// through OpenRouter, while the same rungs 400 against `api.openai.com` (see
/// `openai_effort_ceiling_is_per_model_live`, which pins the other half).
///
/// So OpenRouter is NOT a passthrough for this field — it normalizes onto each
/// upstream's native knob. That is the sentence `docs/config.md` and
/// `EffortWire::Passthrough`'s doc now carry, and the operator-facing consequence is
/// that the same `effort = "max"` is portable on the openrouter kind and model-gated on
/// the gpt kind.
#[tokio::test]
#[ignore = "hits the OpenRouter API; run with --ignored and an OpenRouter key"]
async fn openrouter_clamps_rungs_the_direct_backend_refuses_live() {
    let key = live_key(ProviderKind::OpenRouter);

    for model in ["openai/gpt-5.1", "openai/gpt-5"] {
        for effort in ["xhigh", "max"] {
            let params = json!({ "reasoning": { "effort": effort }, "usage": { "include": true } });
            let resp = openrouter_probe_completion_with(&key, model, &params, CHEAP_PROMPT).await;
            assert_eq!(
                resp["model"].as_str(),
                Some(model),
                "OpenRouter should serve the pinned upstream, not silently reroute: {resp}"
            );
            assert!(
                resp["usage"]["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0,
                "the gateway is expected to translate effort={effort} into something the \
                 upstream honors, billing reasoning tokens: {}",
                resp["usage"]
            );
        }
    }
}

// --- Anthropic — the one unprobed cell -------------------------------------

/// UNVERIFIED. This probe is written and has never been observed to pass or fail on its
/// own logic, because the key available on 2026-08-01 was unfunded: every request to
/// `api.anthropic.com` returned HTTP 400 "Your credit balance is too low to access the
/// Anthropic API" — on `/v1/messages` AND on the otherwise-free
/// `/v1/messages/count_tokens`. The billing check runs *before* body validation, so
/// there is no free path to validate `output_config.effort` on an unfunded account.
/// Anthropic's adaptive ladder is the one cell of the provider matrix still unknown.
///
/// The credit-balance assertion below is deliberately the FIRST thing checked and is a
/// hard failure, not a skip: an unfunded run must be impossible to mistake for a result.
/// This test failing on billing means "we still don't know", which is the honest answer;
/// silently passing would launder an unpaid bill into evidence.
///
/// OpenRouter's Anthropic route settles nothing here either — the gateway is proven
/// above to accept rungs its upstream refuses, so an OpenRouter 200 on `~anthropic/…` at
/// `max` is evidence about OpenRouter, not about Anthropic.
///
/// Run this with a funded key to settle two documents that currently disagree:
/// `docs/config.example.toml` ships `effort = "max"` on an Opus slot, while
/// `src/batch.rs` calls `high` "the proven-accepted top for the Anthropic adaptive
/// tier". Exactly one of those is right. Both are deliberately left as they are until
/// this probe can answer.
#[tokio::test]
#[ignore = "UNVERIFIED: needs a FUNDED Anthropic key; run with --ignored"]
async fn anthropic_adaptive_effort_ladder_live() {
    let key = live_key(ProviderKind::Anthropic);

    for rung in ALL_RUNGS {
        let resp = probe_http()
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": "claude-opus-4-8",
                "max_tokens": 2048,
                "thinking": { "type": "adaptive" },
                "output_config": { "effort": rung },
                "messages": [{ "role": "user", "content": CHEAP_PROMPT }],
            }))
            .send()
            .await
            .expect("anthropic request");
        let status = resp.status();
        let json: Value = resp.json().await.expect("anthropic response json");

        let err = json["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !err.contains("credit balance is too low"),
            "the Anthropic account is unfunded, so this probe cannot distinguish an \
             unsupported rung from an unpaid bill — the ladder is still UNKNOWN, not \
             disproven. Fund the key and re-run. (effort={rung}, {status}: {err})"
        );

        // Record, don't presume. Both outcomes are findings: a 400 names the real
        // ceiling, a 200 with thinking blocks proves the rung reaches the model.
        if status.is_success() {
            let thinking: usize = json["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "thinking")
                        .filter_map(|b| b["thinking"].as_str())
                        .map(str::len)
                        .sum()
                })
                .unwrap_or(0);
            eprintln!(
                "=== effort={rung} ACCEPTED thinking_chars={thinking} usage={}",
                json["usage"]
            );
        } else {
            eprintln!("=== effort={rung} REJECTED ({status}): {}", json["error"]);
        }
    }
}
