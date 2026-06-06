//! Tool gating: each `--no-<tool>` removes exactly its tool, all four on by
//! default, and a server with every tool disabled refuses to start (non-zero exit
//! before serve()). The startup guard is a subprocess test; the per-tool removal
//! is checked directly on the handler's advertised set.

use std::process::Command;

use kaibo::credentials::Provider;
use kaibo::server::{KaiboHandler, ToolGating};

const ALL_TOOLS: [&str; 4] = ["consult", "explore", "run_kaish", "synthesize"];

fn advertised(gating: ToolGating) -> Vec<String> {
    KaiboHandler::new(None, Provider::Anthropic, gating)
        .expect("handler builds")
        .advertised_tools()
}

#[test]
fn default_advertises_all_four_tools() {
    assert_eq!(advertised(ToolGating::default()), ALL_TOOLS);
}

#[test]
fn each_flag_removes_exactly_its_own_tool() {
    let cases = [
        ("consult", ToolGating { consult: false, ..Default::default() }),
        ("explore", ToolGating { explore: false, ..Default::default() }),
        ("synthesize", ToolGating { synthesize: false, ..Default::default() }),
        ("run_kaish", ToolGating { run_kaish: false, ..Default::default() }),
    ];
    for (disabled, gating) in cases {
        let tools = advertised(gating);
        assert!(
            !tools.contains(&disabled.to_string()),
            "{disabled} should be gated off, got {tools:?}"
        );
        // Every *other* tool must still be advertised — gating one doesn't touch the rest.
        for &t in ALL_TOOLS.iter().filter(|&&t| t != disabled) {
            assert!(tools.contains(&t.to_string()), "{t} should remain, got {tools:?}");
        }
    }
}

#[test]
fn all_disabled_is_detected() {
    let none_on = ToolGating { consult: false, explore: false, synthesize: false, run_kaish: false };
    assert!(none_on.all_disabled());
    // Any single tool on means it's a usable server, not the refused state.
    assert!(!ToolGating { run_kaish: true, ..none_on }.all_disabled());
}

/// The startup guard, end to end: launching with all four `--no-*` flags must exit
/// non-zero with a clear message, before binding the stdio transport. A supervisor
/// has to be able to catch a zero-tool misconfiguration.
#[test]
fn all_four_disabled_refuses_to_start() {
    let out = Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .args(["--no-consult", "--no-explore", "--no-synthesize", "--no-run-kaish"])
        .output()
        .expect("should be able to run the kaibo binary");

    assert!(
        !out.status.success(),
        "a zero-tool server must exit non-zero, got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("zero-tool") || stderr.contains("disabled"),
        "the failure must say why; stderr was: {stderr}"
    );
}
