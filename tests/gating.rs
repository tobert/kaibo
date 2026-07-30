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
/// flag. `list_models` is its own gate — a read-only operator/config tool, no model in
/// the loop, independent of everything else here.
const ALL_TOOLS: [&str; 12] = [
    "batch_submit",
    "consult",
    "consult_submit",
    "deliberate",
    "explore",
    "job_cancel",
    "job_get",
    "job_list",
    "job_wait",
    "list_models",
    "oneshot",
    "run_kaish",
];

/// A config where every tool is *staffable*, so these tests measure the `--no-<tool>`
/// flags and nothing else.
///
/// Since a tool no configured cast can staff is now dropped from the router entirely,
/// flag-gating and cast-staffing both decide whether a route survives — and this file is
/// about the flags. So the fixture supplies both cast shapes on one keyless backend (an
/// interactive team, and an explorer paired with an offline synth, which covers
/// `batch_submit` and `deliberate` at once) and neutralizes the built-in casts, whose
/// usability would otherwise depend on which API keys the machine running the tests
/// happens to have. `default_advertises_all_tools` guards the premise.
const FULLY_STAFFED: &str = r#"
    [backends.anthropic]
    api_key_file = "/nonexistent-kaibo-test/anthropic"
    [backends.deepseek]
    api_key_file = "/nonexistent-kaibo-test/deepseek"
    [backends.gemini]
    api_key_file = "/nonexistent-kaibo-test/gemini"
    [backends.openrouter]
    api_key_file = "/nonexistent-kaibo-test/openrouter"
    [backends.openai-local]
    key_optional = false
    api_key_file = "/nonexistent-kaibo-test/openai"

    [backends.gem]
    kind = "gemini"
    key_optional = true

    [casts.inter]
    explorer = "gem/lite"
    synth    = "gem/flash"

    [casts.deep]
    explorer = "gem/lite"
    synth    = { backend = "gem", id = "pro", lane = "batch" }

    [server]
    cast = "inter"
"#;

fn advertised(gating: ToolGating) -> Vec<String> {
    let mut config = Config::from_toml_str(FULLY_STAFFED).expect("fixture config parses");
    config.tools = gating;
    // Empty credential environment: paired with the fixture's unreachable key files, the
    // usable-cast roster is exactly `inter` + `deep`, on any machine.
    KaiboHandler::new_with_env(config, |_| None)
        .expect("handler builds")
        .advertised_tools()
}

/// All flags on and every tool staffable ⇒ the full surface.
///
/// This is also the premise every other test in this file rests on. If a future tool
/// arrives with a cast requirement `FULLY_STAFFED` can't meet, that tool vanishes from
/// the router and the flag assertions below would quietly stop covering it — the
/// `assert_eq!` against the complete list fails instead of silently narrowing.
#[test]
fn default_advertises_all_tools() {
    assert_eq!(advertised(ToolGating::default()), ALL_TOOLS);
}

/// The DOA bug, pinned at the integration level: kaibo's BUILT-IN casts cannot staff
/// `deliberate` (it needs an explorer beside an offline synth, and the built-in offline
/// casts are synth-only), so a stock install must not advertise the tool at all. It used
/// to ship advertised-but-unusable — every call failed `cast "…" has no explorer slot`,
/// while still costing resident tokens in every session.
///
/// Robust to the developer's own keys: adding credentials makes MORE built-in casts
/// usable, but none of them gains an explorer beside an offline synth, so the verdict
/// holds with a full keyring or an empty one.
#[test]
fn a_stock_install_does_not_advertise_deliberate() {
    let tools = KaiboHandler::new(Config::builtin())
        .expect("handler builds")
        .advertised_tools();
    assert!(
        !tools.contains(&"deliberate".to_string()),
        "no built-in cast pairs an explorer with an offline synth, so `deliberate` must \
         not be advertised on a stock install; got {tools:?}"
    );
    assert!(
        tools.contains(&"consult".to_string()),
        "the rest of the surface must survive — this is a targeted drop, not a shutdown; \
         got {tools:?}"
    );
}

#[test]
fn each_flag_removes_exactly_its_own_tools() {
    // Each flag and the tool route(s) it drops *exclusively*. The shared collect verbs
    // `job_get`/`job_cancel`/`job_list` belong to neither flag alone — gating one
    // capability leaves them because the other still needs them — so they appear in no
    // row's removed-set and are covered by the "every other tool remains" check below.
    let cases: [(&[&str], ToolGating); 7] = [
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
            // `--no-explore` drops the single-phase `explore` sweep and nothing else —
            // it's its own gate, independent of consult's driver+explorer loop.
            &["explore"],
            ToolGating {
                explore: false,
                ..Default::default()
            },
        ),
        (
            // `--no-deliberate` drops only `deliberate` — the collect verbs it hands off
            // to (batch/job) stay, gated by their own capability.
            &["deliberate"],
            ToolGating {
                deliberate: false,
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
        (
            // `--no-list-models` drops only `list_models` — an operator/config tool
            // with no model in the loop, independent of every other gate.
            &["list_models"],
            ToolGating {
                list_models: false,
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

/// The shared collect verbs (`job_get`/`job_cancel`/`job_list`/`job_wait`) are gated by
/// *any handle producer*: `consult_submit` and `batch_submit` — and `deliberate`, which
/// produces both a `job-N` (direct lane) and a `backend/provider-id` (batch lane). They
/// stay while any of the three is on, and drop only when all are off. A gate that knew
/// only batch+consult would strand a `deliberate`-only server's handles.
#[test]
fn shared_collect_verbs_track_all_handle_producers() {
    const VERBS: [&str; 4] = ["job_get", "job_cancel", "job_list", "job_wait"];

    // Each producer alone must keep the verbs — its handles need collecting.
    for (label, only) in [
        ("consult", ToolGating { batch: false, deliberate: false, ..Default::default() }),
        ("batch", ToolGating { consult: false, deliberate: false, ..Default::default() }),
        // deliberate alone: it's the case the old batch+consult gate would have stranded.
        ("deliberate", ToolGating { batch: false, consult: false, ..Default::default() }),
    ] {
        let tools = advertised(only);
        for v in VERBS {
            assert!(
                tools.contains(&v.to_string()),
                "{v} must remain with {label} on (it collects {label}'s handles)"
            );
        }
    }

    // All three producers off — nothing to collect, so the verbs drop. (run_kaish/oneshot
    // keep the server a valid, non-empty surface.)
    let none = advertised(ToolGating {
        batch: false,
        consult: false,
        deliberate: false,
        ..Default::default()
    });
    for v in VERBS {
        assert!(
            !none.contains(&v.to_string()),
            "{v} must drop when every handle producer (batch/consult/deliberate) is off"
        );
    }
}

#[test]
fn all_disabled_is_detected() {
    let none_on = ToolGating {
        consult: false,
        explore: false,
        deliberate: false,
        oneshot: false,
        run_kaish: false,
        batch: false,
        list_models: false,
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
    // list_models alone is a usable (if narrow) server — a read-only operator tool
    // with no model in the loop still does something.
    assert!(!ToolGating {
        list_models: true,
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
            "--no-explore",
            "--no-deliberate",
            "--no-oneshot",
            "--no-run-kaish",
            "--no-batch",
            "--no-list-models",
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
