//! Two-phase consult: offline prompt-builder test + an `#[ignore]`d live e2e.

use kaibo::config::{default_models, Config, Profile};
use kaibo::consult::{
    consult, explore, request_params, synthesize, synthesize_user_prompt, thinking_params,
    ConsultConfig, Dialect, ModelShape, Role, ThinkingStyleOverride, DEFAULT_EFFORT,
    THINKING_BUDGET,
};
use kaibo::credentials::{load, ProviderKind};

/// A built-in profile by name, for the live tests below.
fn profile(name: &str) -> Profile {
    Config::builtin()
        .resolve_profile(name)
        .unwrap_or_else(|e| panic!("built-in profile {name:?}: {e}"))
        .clone()
}

#[test]
fn synthesize_prompt_grounds_in_supplied_context() {
    let p = synthesize_user_prompt("What blocks writes?", Some("src/sandbox.rs:95 read-only mount"));

    assert!(p.contains("What blocks writes?"), "question present");
    assert!(p.contains("src/sandbox.rs:95 read-only mount"), "context present");
    assert!(p.to_lowercase().contains("context"), "context labelled");
    // Question framed before the supplied context.
    let q = p.find("What blocks writes?").unwrap();
    let c = p.find("src/sandbox.rs:95 read-only mount").unwrap();
    assert!(q < c, "question should precede the context");
    // Framing: a grounded citation is trusted; `run_kaish` is for *getting more*
    // when the context isn't enough — not for re-verifying what's likely right.
    // Pin both halves so a revert toward "verify the cited span" framing, or a
    // revert that drops the get-more license, fails here.
    assert!(p.contains("run_kaish"), "investigation tool offered even with context");
    let lower = p.to_lowercase();
    assert!(lower.contains("trust"), "a grounded citation should be trusted, got: {p}");
    assert!(
        lower.contains("more than the context")
            || lower.contains("didn't cover")
            || lower.contains("left open"),
        "supplied context must steer toward fetching more, not re-verifying, got: {p}"
    );
}

#[test]
fn synthesize_prompt_without_context_still_points_at_investigation() {
    // The panel's "vacuous with empty context" worry: with no context the prompt
    // must still drive a real investigation via run_kaish, not invite a guess.
    let p = synthesize_user_prompt("What blocks writes?", None);
    assert!(p.contains("What blocks writes?"));
    assert!(
        p.contains("run_kaish"),
        "empty context must still steer to run_kaish, got: {p}"
    );
}

#[test]
fn synthesize_prompt_treats_blank_context_as_absent() {
    // Whitespace-only context is no context — don't pretend there's evidence.
    let p = synthesize_user_prompt("Q?", Some("  \n  "));
    assert!(
        p.contains("run_kaish"),
        "blank context should behave like None, got: {p}"
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

    let adaptive = thinking_params(ProviderKind::Anthropic, "claude-sonnet-4-6", THINKING_BUDGET)
        .expect("anthropic adaptive-tier has a thinking toggle");
    assert_eq!(adaptive["thinking"]["type"], "adaptive");
    assert!(adaptive["thinking"].get("budget_tokens").is_none(), "adaptive carries no budget");

    // Gemini 2.5: nested under generationConfig.thinkingConfig with camelCase keys —
    // rig parses these into a typed GenerationConfig, so the shape must be exact.
    let g = thinking_params(ProviderKind::Gemini, "gemini-2.5-flash", THINKING_BUDGET)
        .expect("gemini has a thinking toggle");
    assert_eq!(
        g["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        THINKING_BUDGET
    );
    assert_eq!(
        g["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}

#[test]
fn gemini_3_line_takes_thinking_level_not_budget() {
    // Gemini 3 officially uses `thinkingLevel` (mutually exclusive with budget); rig's
    // typed ThinkingConfig carries both, serializing the level as snake_case "high".
    // The boundary is empirical: the pure 3-line flips, but `gemini-3.5-flash` (a
    // confirmed-working default on budget) and 2.x stay on budget — switching them
    // would silently regress a request that works (docs/issues.md).
    let g3 = thinking_params(ProviderKind::Gemini, "gemini-3-pro-preview", THINKING_BUDGET)
        .expect("gemini 3 has a thinking toggle");
    let tc = &g3["generationConfig"]["thinkingConfig"];
    assert_eq!(tc["thinkingLevel"], "high", "the 3-line wants a level");
    assert!(tc.get("thinkingBudget").is_none(), "level and budget are mutually exclusive");
    assert_eq!(tc["includeThoughts"], true);

    // The minor 3.5 line and 2.x stay on budget (the conservative, evidence-backed arm).
    for id in ["gemini-3.5-flash", "gemini-2.5-pro"] {
        let g = thinking_params(ProviderKind::Gemini, id, THINKING_BUDGET).expect("toggle");
        let tc = &g["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingBudget"], THINKING_BUDGET, "{id} stays on budget");
        assert!(tc.get("thinkingLevel").is_none(), "{id} must not send a level");
    }
}

#[test]
fn thinking_budget_is_threaded_through_not_hardcoded() {
    // A per-profile budget must reach the request, not the old global constant. Use a
    // budget-tier Anthropic model (Haiku 4.5) — the adaptive tier carries no budget.
    let a = thinking_params(ProviderKind::Anthropic, "claude-haiku-4-5", 4096)
        .expect("anthropic toggle");
    assert_eq!(a["thinking"]["budget_tokens"], 4096);
    let g = thinking_params(ProviderKind::Gemini, "gemini-2.5-flash", 4096).expect("gemini toggle");
    assert_eq!(g["generationConfig"]["thinkingConfig"]["thinkingBudget"], 4096);
}

#[test]
fn request_params_places_sampling_where_each_wire_format_wants_it() {
    // Gemini: temperature + topP (camelCase) ride under generationConfig, alongside
    // the thinkingConfig — one merged blob, not two.
    let g = request_params(ProviderKind::Gemini, "gemini-2.5-flash", THINKING_BUDGET, Some(0.1), Some(0.95))
        .expect("gemini params");
    let gc = &g["generationConfig"];
    assert_eq!(gc["temperature"], 0.1);
    assert_eq!(gc["topP"], 0.95, "Gemini uses camelCase topP");
    assert_eq!(gc["thinkingConfig"]["thinkingBudget"], THINKING_BUDGET, "thinking still rides along");
    assert!(g.get("temperature").is_none(), "must not leak to top level for Gemini");

    // Anthropic: thinking wins — sampling is dropped (any tier). The Messages API 400s on
    // any `temperature != 1` (and restricts top_p/top_k) whenever thinking is enabled
    // ("temperature may only be set to 1 when thinking is enabled"). Thinking is the
    // higher-value default (AGENTS.md "Driving the models"), so we keep it and let the
    // per-role sampling go inert rather than 400 the request. (Budget tier here; the
    // adaptive tier drops sampling the same way — see anthropic_newest_models_*.)
    let a = request_params(ProviderKind::Anthropic, "claude-haiku-4-5", 4096, Some(0.3), Some(0.95))
        .expect("anthropic params");
    assert_eq!(a["thinking"]["budget_tokens"], 4096);
    assert!(a.get("temperature").is_none(), "temperature must not ride alongside thinking for Anthropic");
    assert!(a.get("top_p").is_none(), "top_p must not ride alongside thinking for Anthropic");

    // OpenAI: no thinking toggle, but sampling still goes top-level.
    let o = request_params(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", 0, Some(0.2), Some(0.9))
        .expect("openai sampling");
    assert_eq!(o["temperature"], 0.2);
    assert_eq!(o["top_p"], 0.9);
    assert!(o.get("thinking").is_none());

    // DeepSeek: thinking toggle and sampling coexist, all top-level.
    let d = request_params(ProviderKind::DeepSeek, "deepseek-v4-pro", THINKING_BUDGET, Some(0.3), Some(0.95))
        .expect("deepseek params");
    assert_eq!(d["thinking"]["type"], "enabled");
    assert_eq!(d["reasoning_effort"], "high");
    assert_eq!(d["temperature"], 0.3);
    assert_eq!(d["top_p"], 0.95);

    // Nothing set anywhere → None (the leaf passes no additional_params at all).
    assert!(request_params(ProviderKind::Openai, "m", 0, None, None).is_none());
}

#[test]
fn anthropic_thinking_suppresses_sampling_or_the_api_400s() {
    // Regression for the live 400: "temperature may only be set to 1 when thinking is
    // enabled". Anthropic rejects custom temperature (and restricts top_p/top_k) while
    // extended thinking is on. Thinking is on for every Anthropic profile by default,
    // and temperature has a default on every profile, so the per-role sampling feature
    // collides with thinking on the very first call. Thinking wins; sampling drops.
    // (Budget tier shown; the adaptive tier drops sampling identically.)
    let a = request_params(ProviderKind::Anthropic, "claude-haiku-4-5", THINKING_BUDGET, Some(0.3), Some(0.95))
        .expect("anthropic still sends the thinking block");
    assert_eq!(a["thinking"]["type"], "enabled");
    assert_eq!(a["thinking"]["budget_tokens"], THINKING_BUDGET);
    assert!(a.get("temperature").is_none(), "temperature would 400 alongside thinking");
    assert!(a.get("top_p").is_none(), "top_p is dropped with it, for the same reason");

    // The suppression is keyed on whether the model accepts sampling under thinking, not
    // on the provider alone: contrast DeepSeek, which also enables thinking but *accepts*
    // sampling, so its knobs ride through untouched.
    let d = request_params(ProviderKind::DeepSeek, "deepseek-v4-pro", THINKING_BUDGET, Some(0.3), Some(0.95))
        .expect("deepseek params");
    assert_eq!(d["temperature"], 0.3, "DeepSeek keeps sampling even with thinking on");
    assert_eq!(d["top_p"], 0.95);
}

#[test]
fn anthropic_newest_models_get_adaptive_thinking() {
    // Opus 4.7/4.8 and Fable 5 *require* adaptive: enabled/budget AND temperature/top_p
    // all 400. Each gets `{thinking:{type:adaptive}, output_config:{effort}}` and no
    // sampling. (Mirrors the live "fix anthropic" goal — these would 400 on the old shape.)
    for model in ["claude-opus-4-8", "claude-opus-4-7", "claude-fable-5"] {
        let a = request_params(ProviderKind::Anthropic, model, THINKING_BUDGET, Some(0.3), Some(0.95))
            .unwrap_or_else(|| panic!("{model} sends a thinking block"));
        assert_eq!(a["thinking"]["type"], "adaptive", "{model} wants adaptive");
        assert!(a["thinking"].get("budget_tokens").is_none(), "{model} rejects budget_tokens");
        assert_eq!(a["output_config"]["effort"], DEFAULT_EFFORT, "{model} carries effort");
        assert!(a.get("temperature").is_none(), "{model} rejects temperature");
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
    for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-fable-5"] {
        assert_eq!(thinking_type(model), "adaptive", "{model} is the adaptive tier");
    }
    for model in ["claude-haiku-4-5", "claude-opus-4-5", "claude-sonnet-4-5", "claude-opus-4-1"] {
        assert_eq!(thinking_type(model), "enabled", "{model} stays on enabled/budget");
    }
}

#[test]
fn effort_is_per_role_and_provider_mapped() {
    // Effort lands only where the model takes it, at that provider's wire field.
    let anthropic = ModelShape::resolve(ProviderKind::Anthropic, "claude-opus-4-8", ThinkingStyleOverride::Auto)
        .to_params(THINKING_BUDGET, None, None, "xhigh")
        .expect("adaptive params");
    assert_eq!(anthropic["output_config"]["effort"], "xhigh");

    let deepseek = ModelShape::resolve(ProviderKind::DeepSeek, "deepseek-v4-pro", ThinkingStyleOverride::Auto)
        .to_params(THINKING_BUDGET, None, None, "max")
        .expect("deepseek params");
    assert_eq!(deepseek["reasoning_effort"], "max");

    // Budget-tier Anthropic and Gemini have no effort sink — the value is ignored, not leaked.
    let budget = ModelShape::resolve(ProviderKind::Anthropic, "claude-haiku-4-5", ThinkingStyleOverride::Auto)
        .to_params(THINKING_BUDGET, None, None, "xhigh")
        .expect("budget params");
    assert!(budget.get("output_config").is_none(), "budget tier has no effort sink");
    let gemini = ModelShape::resolve(ProviderKind::Gemini, "gemini-2.5-flash", ThinkingStyleOverride::Auto)
        .to_params(THINKING_BUDGET, None, None, "xhigh")
        .expect("gemini params");
    assert!(gemini.get("reasoning_effort").is_none(), "Gemini has no effort sink");
    assert!(gemini.get("output_config").is_none());

    // Per-role resolution through the Dialect: explorer and synth get their own effort.
    let mut p = profile("anthropic");
    p.explorer_effort = "low".into();
    p.synth_effort = "max".into();
    let dialect = Dialect::from_profile(&p);
    let explorer = dialect.request_params("claude-opus-4-8", Role::Explorer).expect("explorer params");
    let synth = dialect.request_params("claude-opus-4-8", Role::Synth).expect("synth params");
    assert_eq!(explorer["output_config"]["effort"], "low", "explorer effort");
    assert_eq!(synth["output_config"]["effort"], "max", "synth effort");
}

#[test]
fn thinking_style_override_forces_a_tier() {
    // The escape hatch both ways: force an older model onto adaptive, or a newest model
    // back onto budget, when the classifier is wrong or a new id ships.
    let forced_adaptive = ModelShape::resolve(ProviderKind::Anthropic, "claude-sonnet-4-5", ThinkingStyleOverride::Adaptive)
        .to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT)
        .expect("forced adaptive");
    assert_eq!(forced_adaptive["thinking"]["type"], "adaptive", "override beats the classifier");

    let forced_budget = ModelShape::resolve(ProviderKind::Anthropic, "claude-opus-4-8", ThinkingStyleOverride::Budget)
        .to_params(THINKING_BUDGET, None, None, DEFAULT_EFFORT)
        .expect("forced budget");
    assert_eq!(forced_budget["thinking"]["type"], "enabled", "override beats the classifier");
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
    assert!(d.get("generationConfig").is_none(), "DeepSeek is not Gemini-shaped");
}

#[test]
fn the_generic_openai_path_sends_no_thinking_toggle() {
    // The local Gemma default (its --reasoning-format auto) already reasons, and
    // there's no portable toggle across arbitrary OpenAI-compatible endpoints: None.
    assert!(thinking_params(ProviderKind::Openai, "Gemma-4-E4B-it-GGUF", THINKING_BUDGET).is_none());
}

#[test]
fn default_config_gives_large_headroom_above_the_thinking_budget() {
    let cfg = ConsultConfig::default();
    // Amy's default: few high-value turns, generous output budget.
    assert!(
        cfg.max_tokens >= 16384,
        "want generous headroom, got {}",
        cfg.max_tokens
    );
    // Anthropic requires max_tokens strictly greater than the thinking budget.
    assert!(cfg.max_tokens > THINKING_BUDGET);
}

#[test]
fn builtin_openai_profile_is_cheap_gemma_explorer_strong_gemma_synth() {
    // The chosen mapping for the local default endpoint: small E4B drives the
    // tool-heavy exploration, the larger 26B writes the answer — the cheap-
    // explorer/strong-synth pattern, local edition.
    let (explorer, synth) = default_models(ProviderKind::Openai);
    assert_eq!(explorer, "Gemma-4-E4B-it-GGUF");
    assert_eq!(synth, "Gemma-4-26B-A4B-it-GGUF");

    // And the built-in `openai` profile resolves to exactly those.
    let p = profile("openai");
    assert_eq!(p.explorer_model, explorer);
    assert_eq!(p.synth_model, synth);
    assert_eq!(p.kind, ProviderKind::Openai);
}

// Validation of the secondary-profile path: load the real user config
// (~/.config/kaibo/config.toml), resolve a *non-built-in* openai profile, and run
// its synth model against Lemonade. Proves config → profile → client wiring for a
// second model on the same provider — the headline feature, exercised live.
#[tokio::test]
#[ignore = "loads ~/.config/kaibo/config.toml and hits local Lemonade (GLM); run with --ignored while it's up"]
async fn secondary_local_profile_from_user_config_runs() {
    // Part 1 — the user config file parses and its extra profiles resolve to the
    // right model + endpoint (no network).
    let cfg = Config::load(None).expect("load user config from the XDG default path");
    let glm = cfg
        .resolve_profile("glm")
        .expect("the user config should define a `glm` profile")
        .clone();
    assert_eq!(glm.synth_model, "GLM-4.5-Air-UD-Q4K-XL-GGUF");
    assert_eq!(glm.resolved_base_url(), "http://localhost:13305/api/v1");
    assert_eq!(glm.kind, ProviderKind::Openai);
    let qwen = cfg.resolve_profile("qwen").expect("a `qwen` profile");
    assert_eq!(qwen.synth_model, "Qwen3-Coder-Next-GGUF");

    // Part 2 — run a *second* profile live to prove the resolve → client → call path
    // end-to-end. The GLM/Qwen builds don't always load on Lemonade; Gemma does, so
    // validate against a profile that selects a Gemma synth distinct from any
    // built-in default. (If GLM/Qwen are loadable, point synth_model back at them.)
    let mut secondary = glm.clone();
    secondary.synth_model = "Gemma-4-E4B-it-GGUF".to_string();
    let answer = synthesize(
        "In one sentence, what does kaibo's read-only sandbox prevent?",
        Some("src/sandbox.rs builds a read-only kernel; writes and external commands are refused."),
        env!("CARGO_MANIFEST_DIR"),
        &secondary,
        &ConsultConfig::default(),
    )
    .await
    .expect("secondary-profile synthesize against Lemonade should succeed");
    eprintln!("=== SECONDARY PROFILE ANSWER ===\n{answer}\n");
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
        root,
        &profile("openai"),
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

// The `explore` unit on its own (the seam behind the MCP `explore` tool): a cheap
// model drives {run_kaish} and returns a curated report citing real file:line.
#[tokio::test]
#[ignore = "hits the local OpenAI-compatible (Gemma) server (explore only); run with --ignored while it's up"]
async fn explore_unit_reports_from_the_real_tree() {
    let report = explore(
        "Which source file enforces the read-only sandbox, and name one builtin it blocks?",
        env!("CARGO_MANIFEST_DIR"),
        &profile("openai"),
        &ConsultConfig::default(),
    )
    .await
    .expect("explore against local gemma should succeed");

    eprintln!("=== EXPLORE REPORT ===\n{report}\n");
    let lower = report.to_lowercase();
    assert!(
        lower.contains("sandbox.rs") || lower.contains("sandbox"),
        "the report should cite the sandbox source, got: {report}"
    );
}

// Standalone `synthesize` (the seam behind the MCP `synthesize` tool): grounded
// from supplied context, and — the panel's worry — still useful with no context
// because run_kaish lets it investigate rather than guess.
#[tokio::test]
#[ignore = "hits the local OpenAI-compatible (Gemma) server (synthesize); run with --ignored while it's up"]
async fn synthesize_grounds_in_context_and_investigates_without_it() {
    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();
    let p = profile("openai");

    // With a thin context: it should answer grounded, optionally confirming via run_kaish.
    let with_ctx = synthesize(
        "Which file enforces the read-only sandbox?",
        Some("src/sandbox.rs builds a read-only kernel; the LocalFs read-only mount refuses every write."),
        root,
        &p,
        &cfg,
    )
    .await
    .expect("synthesize with context should succeed");
    eprintln!("=== SYNTH (with context) ===\n{with_ctx}\n");
    assert!(
        with_ctx.to_lowercase().contains("sandbox"),
        "should answer about the sandbox file, got: {with_ctx}"
    );

    // With NO context: it must still investigate via run_kaish and answer grounded.
    let no_ctx = synthesize(
        "Which file enforces the read-only sandbox?",
        None,
        root,
        &p,
        &cfg,
    )
    .await
    .expect("synthesize without context should still investigate and succeed");
    eprintln!("=== SYNTH (no context) ===\n{no_ctx}\n");
    assert!(
        no_ctx.to_lowercase().contains("sandbox"),
        "empty-context synth must investigate and still answer, got: {no_ctx}"
    );
}

// Live thinking-on checks for the keyed providers. They exercise the risky paths:
// Anthropic's thinking blocks round-tripping through the tool loop, and Gemini's
// thinkingConfig shape (thinkingBudget vs the Gemini-3 thinkingLevel split).
#[tokio::test]
#[ignore = "hits the DeepSeek API (explore + synth); run with --ignored and a key"]
async fn two_phase_consult_via_deepseek() {
    if let Err(e) = load(ProviderKind::DeepSeek) {
        panic!("no DeepSeek credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        env!("CARGO_MANIFEST_DIR"),
        &profile("deepseek"),
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
#[ignore = "hits the Gemini API (explore + synth); run with --ignored and a key"]
async fn two_phase_consult_via_gemini() {
    if let Err(e) = load(ProviderKind::Gemini) {
        panic!("no Gemini credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        env!("CARGO_MANIFEST_DIR"),
        &profile("gemini"),
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
#[ignore = "hits the Anthropic API (explore + synth); run with --ignored and a key"]
async fn two_phase_consult_answers_from_the_real_tree() {
    // Surface a clear message if the credential is missing, before the API call.
    if let Err(e) = load(ProviderKind::Anthropic) {
        panic!("no Anthropic credential for live test: {e}");
    }

    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        root,
        &profile("anthropic"),
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
#[ignore = "hits the Anthropic API on Opus 4.8 (adaptive-only tier); run with --ignored and a key"]
async fn adaptive_only_anthropic_model_round_trips() {
    // The real proof of the "fix anthropic" work: Opus 4.8 *rejects* the old
    // enabled/budget thinking block AND sampling outright (400). Pin both phases to it so
    // a regression in the adaptive shape (or the output_config.effort flatten through rig)
    // surfaces as a live 400 here, not in production — what the offline tests can't prove.
    if let Err(e) = load(ProviderKind::Anthropic) {
        panic!("no Anthropic credential for live test: {e}");
    }

    let mut p = profile("anthropic");
    p.explorer_model = "claude-opus-4-8".into();
    p.synth_model = "claude-opus-4-8".into();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        env!("CARGO_MANIFEST_DIR"),
        &p,
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
