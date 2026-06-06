//! Two-phase consult: offline prompt-builder test + an `#[ignore]`d live e2e.

use kaibo::consult::{
    consult, default_models, explore, synth_user_prompt, thinking_params, ConsultConfig,
    THINKING_BUDGET,
};
use kaibo::credentials::{load, Provider};

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
fn thinking_is_enabled_for_providers_with_a_request_toggle() {
    // Anthropic: extended thinking via a top-level `thinking` block (rig flattens
    // additional_params into the Messages request).
    let a = thinking_params(Provider::Anthropic).expect("anthropic has a thinking toggle");
    assert_eq!(a["thinking"]["type"], "enabled");
    assert_eq!(a["thinking"]["budget_tokens"], THINKING_BUDGET);

    // Gemini: nested under generationConfig.thinkingConfig with camelCase keys —
    // rig parses these into a typed GenerationConfig, so the shape must be exact.
    let g = thinking_params(Provider::Gemini).expect("gemini has a thinking toggle");
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
fn providers_that_reason_without_a_toggle_get_no_params() {
    // DeepSeek reasoner models and local Gemma (lemonade's --reasoning-format auto)
    // already reason; there is no request-time switch to flip, so: None.
    assert!(thinking_params(Provider::DeepSeek).is_none());
    assert!(thinking_params(Provider::Lemonade).is_none());
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
fn lemonade_defaults_to_cheap_gemma_explorer_strong_gemma_synth() {
    // The chosen mapping: small E4B drives the tool-heavy exploration, the larger
    // 26B writes the answer — the cheap-explorer/strong-synth pattern, local edition.
    let (explorer, synth) = default_models(Provider::Lemonade);
    assert_eq!(explorer, "Gemma-4-E4B-it-GGUF");
    assert_eq!(synth, "Gemma-4-26B-A4B-it-GGUF");

    // And an unset config resolves to exactly those.
    let cfg = ConsultConfig::default();
    assert_eq!(
        cfg.resolved_models(Provider::Lemonade),
        (explorer.to_string(), synth.to_string())
    );
}

#[tokio::test]
#[ignore = "hits the local lemonade server (explore + synth); run with --ignored while it's up"]
async fn two_phase_consult_runs_against_local_gemma() {
    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        root,
        Provider::Lemonade,
        &cfg,
    )
    .await
    .expect("consult against local gemma should succeed");

    eprintln!("=== REPORT (gemma explorer) ===\n{}\n", out.report);
    eprintln!("=== ANSWER (gemma synth) ===\n{}\n", out.answer);

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
#[ignore = "hits the local lemonade server (explore only); run with --ignored while it's up"]
async fn explore_unit_reports_from_the_real_tree() {
    let report = explore(
        "Which source file enforces the read-only sandbox, and name one builtin it blocks?",
        env!("CARGO_MANIFEST_DIR"),
        Provider::Lemonade,
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

// Live thinking-on checks for the keyed providers. They exercise the risky paths:
// Anthropic's thinking blocks round-tripping through the tool loop, and Gemini's
// thinkingConfig shape (thinkingBudget vs the Gemini-3 thinkingLevel split).
#[tokio::test]
#[ignore = "hits the DeepSeek API (explore + synth); run with --ignored and a key"]
async fn two_phase_consult_via_deepseek() {
    if let Err(e) = load(Provider::DeepSeek) {
        panic!("no DeepSeek credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        env!("CARGO_MANIFEST_DIR"),
        Provider::DeepSeek,
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
    if let Err(e) = load(Provider::Gemini) {
        panic!("no Gemini credential for live test: {e}");
    }
    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        env!("CARGO_MANIFEST_DIR"),
        Provider::Gemini,
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
    if let Err(e) = load(Provider::Anthropic) {
        panic!("no Anthropic credential for live test: {e}");
    }

    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ConsultConfig::default();

    let out = consult(
        "How does kaibo stop the explorer from deleting real files? Name the mechanism and the file.",
        root,
        Provider::Anthropic,
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
