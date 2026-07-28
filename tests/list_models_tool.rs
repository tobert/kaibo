//! The caller-facing `list_models` #[tool], driven directly through `KaiboHandler` (the
//! same pattern `tests/run_kaish_tool.rs` uses) — offline only. A handler built with
//! zero configured backends sweeps nothing, so this never touches the network; it's
//! here to prove `structured_content` is wired up to `models_json_envelope`, not to
//! exercise a real provider (that needs a live backend and belongs to a manual probe,
//! not `cargo test`).

use kaibo::config::Config;
use kaibo::server::{KaiboHandler, ListModelsInput};
use rmcp::handler::server::wrapper::Parameters;

#[tokio::test]
async fn list_models_attaches_structured_content_mirroring_the_json_envelope() {
    // Zero backends: the sweep loop in `list_models` iterates `config.backends.keys()`,
    // so an empty map means zero fetches and zero network calls — offline by
    // construction, not by mocking.
    let mut config = Config::builtin();
    config.backends.clear();
    let handler = KaiboHandler::new(config).expect("handler builds with zero backends");

    let result = handler
        .list_models(Parameters(ListModelsInput { backend: None }))
        .await
        .expect("list_models tool call should not be an MCP-level error");

    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({ "backends": [] })),
        "structured_content should mirror the CLI's --json envelope for an empty sweep"
    );

    // The prose face still renders too — the two outputs coexist, they don't replace
    // each other.
    let text = result
        .content
        .into_iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("no backend configured"), "got: {text}");
}
