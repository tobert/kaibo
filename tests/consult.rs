//! Two-phase consult: offline prompt-builder test + an `#[ignore]`d live e2e.

use kaibo::config::{default_models, Config, Profile};
use kaibo::consult::{
    consult, explore, synth_user_prompt, synthesize, synthesize_user_prompt, thinking_params,
    ConsultConfig, THINKING_BUDGET,
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
fn synth_prompt_carries_question_and_report() {
    let p = synth_user_prompt("What blocks writes?", "src/sandbox.rs: read-only mount");

    // Both halves of the hand-off must be present and labelled, so the synth
    // can tell the user's question from the explorer's evidence.
    assert!(p.contains("What blocks writes?"));
    assert!(p.contains("src/sandbox.rs: read-only mount"));
    assert!(p.contains("Question:"));
    assert!(p.contains("report"));
    // The question must appear before the report (framing order matters).
    let q = p.find("What blocks writes?").unwrap();
    let r = p.find("src/sandbox.rs: read-only mount").unwrap();
    assert!(q < r, "question should precede the report in the prompt");
}

#[test]
fn thinking_is_enabled_for_kinds_with_a_request_toggle() {
    // Anthropic: extended thinking via a top-level `thinking` block (rig flattens
    // additional_params into the Messages request). The budget is now a parameter.
    let a = thinking_params(ProviderKind::Anthropic, THINKING_BUDGET)
        .expect("anthropic has a thinking toggle");
    assert_eq!(a["thinking"]["type"], "enabled");
    assert_eq!(a["thinking"]["budget_tokens"], THINKING_BUDGET);

    // Gemini: nested under generationConfig.thinkingConfig with camelCase keys —
    // rig parses these into a typed GenerationConfig, so the shape must be exact.
    let g = thinking_params(ProviderKind::Gemini, THINKING_BUDGET)
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
fn thinking_budget_is_threaded_through_not_hardcoded() {
    // A per-profile budget must reach the request, not the old global constant.
    let a = thinking_params(ProviderKind::Anthropic, 4096).expect("anthropic toggle");
    assert_eq!(a["thinking"]["budget_tokens"], 4096);
    let g = thinking_params(ProviderKind::Gemini, 4096).expect("gemini toggle");
    assert_eq!(g["generationConfig"]["thinkingConfig"]["thinkingBudget"], 4096);
}

#[test]
fn kinds_that_reason_without_a_toggle_get_no_params() {
    // DeepSeek reasoner models and the local Gemma default (its --reasoning-format
    // auto) already reason; there is no request-time switch to flip, so: None.
    assert!(thinking_params(ProviderKind::DeepSeek, THINKING_BUDGET).is_none());
    assert!(thinking_params(ProviderKind::Openai, THINKING_BUDGET).is_none());
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
        Some("src/sandbox.rs builds a read-only kernel; the DENYLIST shadow-blocks touch/git."),
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
