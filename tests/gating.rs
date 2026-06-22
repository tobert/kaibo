//! Tool gating: each `--no-<tool>` removes exactly its tool, all tools on by
//! default, and a server with every tool disabled refuses to start (non-zero exit
//! before serve()). The startup guard is a subprocess test; the per-tool removal
//! is checked directly on the handler's advertised set.

use std::process::Command;

use kaibo::config::Config;
use kaibo::server::{KaiboHandler, ToolGating};

/// Every advertised tool, sorted (the order `advertised_tools` returns). The batch
/// capability is four routes (`batch_submit`/`batch_get`/`batch_cancel`/`batch_list`)
/// behind one `--no-batch` flag.
const ALL_TOOLS: [&str; 8] = [
    "batch_cancel",
    "batch_get",
    "batch_list",
    "batch_submit",
    "consult",
    "generate_image",
    "oneshot",
    "run_kaish",
];

fn advertised(gating: ToolGating) -> Vec<String> {
    let mut config = Config::builtin();
    config.tools = gating;
    KaiboHandler::new(config)
        .expect("handler builds")
        .advertised_tools()
}

#[test]
fn default_advertises_all_tools() {
    assert_eq!(advertised(ToolGating::default()), ALL_TOOLS);
}

#[test]
fn each_flag_removes_exactly_its_own_tools() {
    // Each flag and the tool route(s) it drops. `--no-batch` is one flag over the
    // whole batch trio — they're one capability — so it lists all three.
    let cases: [(&[&str], ToolGating); 5] = [
        (
            &["consult"],
            ToolGating {
                consult: false,
                ..Default::default()
            },
        ),
        (
            &["oneshot"],
            ToolGating {
                oneshot: false,
                ..Default::default()
            },
        ),
        (
            &["run_kaish"],
            ToolGating {
                run_kaish: false,
                ..Default::default()
            },
        ),
        (
            &["generate_image"],
            ToolGating {
                generate_image: false,
                ..Default::default()
            },
        ),
        (
            &["batch_submit", "batch_get", "batch_cancel", "batch_list"],
            ToolGating {
                batch: false,
                ..Default::default()
            },
        ),
    ];
    for (disabled, gating) in cases {
        let tools = advertised(gating);
        for d in disabled {
            assert!(
                !tools.contains(&d.to_string()),
                "{d} should be gated off, got {tools:?}"
            );
        }
        // Every *other* tool must still be advertised — gating one doesn't touch the rest.
        for &t in ALL_TOOLS.iter().filter(|t| !disabled.contains(t)) {
            assert!(
                tools.contains(&t.to_string()),
                "{t} should remain, got {tools:?}"
            );
        }
    }
}

#[test]
fn all_disabled_is_detected() {
    let none_on = ToolGating {
        consult: false,
        oneshot: false,
        run_kaish: false,
        generate_image: false,
        batch: false,
    };
    assert!(none_on.all_disabled());
    // Any single tool on means it's a usable server, not the refused state.
    assert!(!ToolGating {
        run_kaish: true,
        ..none_on
    }
    .all_disabled());
    // The batch capability alone is enough to be a usable server.
    assert!(!ToolGating {
        batch: true,
        ..none_on
    }
    .all_disabled());
}

/// The startup guard, end to end: launching with every `--no-*` flag must exit
/// non-zero with a clear message, before binding the stdio transport. A supervisor
/// has to be able to catch a zero-tool misconfiguration.
#[test]
fn all_tools_disabled_refuses_to_start() {
    // Isolate from the developer's real ~/.config/kaibo/config.toml: point
    // XDG_CONFIG_HOME at an empty dir so the binary runs on built-ins and the
    // failure under test (zero tools) is the only one in play.
    let empty_config = tempfile::tempdir().expect("tempdir for an isolated XDG_CONFIG_HOME");
    let out = Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env("XDG_CONFIG_HOME", empty_config.path())
        .args([
            "--no-consult",
            "--no-oneshot",
            "--no-run-kaish",
            "--no-generate-image",
            "--no-batch",
        ])
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
