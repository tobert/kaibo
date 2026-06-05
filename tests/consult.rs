//! Two-phase consult: offline prompt-builder test + an `#[ignore]`d live e2e.

use kaibo::consult::{consult, default_models, synth_user_prompt, ConsultConfig};
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
