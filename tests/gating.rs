//! Tool gating: each `--no-<tool>` removes exactly its tool, all tools on by
//! default, and a server with every tool disabled refuses to start (non-zero exit
//! before serve()). The startup guard is a subprocess test; the per-tool removal
//! is checked directly on the handler's advertised set.

use std::process::Command;

use kaibo::config::Config;
use kaibo::server::{KaiboHandler, ToolGating};

/// Every advertised tool, sorted (the order `advertised_tools` returns). `consult` and
/// `batch` each carry a `*_submit` under their own flag; the collect verbs `job_get`/
/// `job_cancel`/`job_list`/`job_wait` are *shared* — they manage both kinds of handle
/// and stay advertised as long as either capability is on, so they belong to neither
/// flag.
const ALL_TOOLS: [&str; 9] = [
    "batch_submit",
    "consult",
    "consult_submit",
    "job_cancel",
    "job_get",
    "job_list",
    "job_wait",
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
    // Each flag and the tool route(s) it drops *exclusively*. The shared collect verbs
    // `job_get`/`job_cancel`/`job_list` belong to neither flag alone — gating one
    // capability leaves them because the other still needs them — so they appear in no
    // row's removed-set and are covered by the "every other tool remains" check below.
    let cases: [(&[&str], ToolGating); 4] = [
        (
            // `--no-consult` drops the blocking `consult` and the async `consult_submit`;
            // `job_get`/`job_cancel`/`job_list` stay (batch still uses them).
            &["consult", "consult_submit"],
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
            // `--no-batch` drops only `batch_submit`; `job_get`/`job_cancel`/`job_list`
            // stay (consult still uses them).
            &["batch_submit"],
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

/// The shared collect verbs (`job_get`/`job_cancel`/`job_list`) are gated by *either*
/// capability: they stay while batch or consult is on, and drop only when both are
/// off. A naive single-flag gate would fail the "stays with the other capability on"
/// cases.
#[test]
fn shared_collect_verbs_track_both_capabilities() {
    const VERBS: [&str; 4] = ["job_get", "job_cancel", "job_list", "job_wait"];

    // consult on, batch off — the verbs still serve consult jobs.
    let consult_only = advertised(ToolGating {
        batch: false,
        ..Default::default()
    });
    for v in VERBS {
        assert!(
            consult_only.contains(&v.to_string()),
            "{v} must remain with consult on (it collects consult jobs)"
        );
    }

    // batch on, consult off — the verbs still serve batches.
    let batch_only = advertised(ToolGating {
        consult: false,
        ..Default::default()
    });
    for v in VERBS {
        assert!(
            batch_only.contains(&v.to_string()),
            "{v} must remain with batch on (it collects batches)"
        );
    }

    // Both off — nothing to collect, so the verbs drop. (run_kaish/oneshot keep the
    // server a valid, non-empty surface.)
    let neither = advertised(ToolGating {
        batch: false,
        consult: false,
        ..Default::default()
    });
    for v in VERBS {
        assert!(
            !neither.contains(&v.to_string()),
            "{v} must drop when both batch and consult are off"
        );
    }
}

#[test]
fn all_disabled_is_detected() {
    let none_on = ToolGating {
        consult: false,
        oneshot: false,
        run_kaish: false,
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
