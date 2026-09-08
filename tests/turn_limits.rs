//! The turn budget is a server property, not a per-call argument.
//!
//! `explorer_max_turns` and `synth_max_turns` live in `[defaults]` (and their `KAIBO_*`
//! env spellings), where the operator sets them once for the server. They were once tool
//! params and CLI flags too, and a calling agent that could see them tuned them — raising
//! a cap is the reflex a stalled model reaches for, and it bought nothing kaibo's own
//! defaults and its finalize-after-max-turns recovery do not already handle, while making
//! two calls to the same server incomparable.
//!
//! So the per-call road is closed, and closed *loudly*: with the fields gone,
//! `deny_unknown_fields` turns a tuning attempt into an invalid-params error naming the
//! field, never a silent drop into the configured default. These tests pin that end
//! state on both front doors.

use clap::Parser;
use kaibo::cli::Cli;
use kaibo::config::Config;
use kaibo::server::{ConsultInput, DeliberateInput, ExploreInput};
use serde_json::json;

/// Every tool that runs a model loop refuses a per-call turn limit by name.
#[test]
fn the_mcp_tools_refuse_a_per_call_turn_limit() {
    let cases: Vec<(&str, &str, Option<String>)> = vec![
        (
            "consult",
            "explorer_max_turns",
            serde_json::from_value::<ConsultInput>(
                json!({ "question": "q", "explorer_max_turns": 5 }),
            )
            .err()
            .map(|e| e.to_string()),
        ),
        (
            "consult",
            "synth_max_turns",
            serde_json::from_value::<ConsultInput>(
                json!({ "question": "q", "synth_max_turns": 500 }),
            )
            .err()
            .map(|e| e.to_string()),
        ),
        (
            "explore",
            "explorer_max_turns",
            serde_json::from_value::<ExploreInput>(
                json!({ "question": "q", "explorer_max_turns": 5 }),
            )
            .err()
            .map(|e| e.to_string()),
        ),
        (
            "deliberate",
            "explorer_max_turns",
            serde_json::from_value::<DeliberateInput>(
                json!({ "question": "q", "explorer_max_turns": 5 }),
            )
            .err()
            .map(|e| e.to_string()),
        ),
    ];

    for (tool, field, err) in cases {
        let msg = err.unwrap_or_else(|| {
            panic!("{tool}: `{field}` must be refused, not accepted as a per-call override")
        });
        assert!(
            msg.contains(field),
            "{tool}: the refusal must name `{field}`, got: {msg}"
        );
    }
}

/// The CLI closes the same road: the flags do not parse, so a script or an agent
/// reaching for one exits 2 with usage rather than running on a tuned budget.
#[test]
fn the_cli_has_no_turn_limit_flags() {
    for argv in [
        vec!["kaibo", "consult", "why?", "--explorer-max-turns", "5"],
        vec!["kaibo", "consult", "why?", "--synth-max-turns", "500"],
        vec!["kaibo", "explore", "map it", "--explorer-max-turns", "5"],
        vec![
            "kaibo",
            "deliberate",
            "is this right?",
            "--explorer-max-turns",
            "5",
        ],
    ] {
        let flag = argv[3];
        Cli::try_parse_from(&argv)
            .err()
            .unwrap_or_else(|| panic!("`{flag}` must not parse: {argv:?}"));
    }
}

/// The operator keeps the knob. Closing the per-call road moves the decision to the
/// server's config, so the defaults must still be there to set.
#[test]
fn the_operator_still_sets_the_budget_in_config() {
    let d = Config::builtin().defaults;
    assert_eq!(d.explorer_max_turns, 100);
    assert_eq!(d.synth_max_turns, 200);
}
