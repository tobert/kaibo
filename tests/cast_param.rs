//! The `cast` call param (docs/casts.md): the canonical selector on all three
//! model-driven tools, exercised at the exact seam rmcp deserializes tool
//! arguments through. The old `provider` spelling carried a transitional
//! `#[serde(alias = "provider")]` for one cycle after the backends/casts rename;
//! that alias is now removed, so a stale `provider` falls under
//! `deny_unknown_fields` and becomes a loud invalid-params error — never a silent
//! drop into the default cast (serde drops unknown fields, a textbook silent
//! fallback). These tests pin that end state.

use kaibo::server::{ConsultInput, ExploreInput, RunKaishInput, SynthesizeInput};
use serde_json::json;

#[test]
fn cast_is_the_canonical_spelling_and_optional() {
    let c: ConsultInput = serde_json::from_value(json!({ "question": "q", "cast": "chimera" }))
        .expect("consult takes cast");
    assert_eq!(c.cast.as_deref(), Some("chimera"));

    let e: ExploreInput = serde_json::from_value(json!({ "question": "q", "cast": "chimera" }))
        .expect("explore takes cast");
    assert_eq!(e.cast.as_deref(), Some("chimera"));

    let s: SynthesizeInput = serde_json::from_value(json!({ "question": "q", "cast": "chimera" }))
        .expect("synthesize takes cast");
    assert_eq!(s.cast.as_deref(), Some("chimera"));

    // Omitting it entirely falls through to the server's default cast.
    let d: ConsultInput =
        serde_json::from_value(json!({ "question": "q" })).expect("cast is optional");
    assert!(d.cast.is_none());
}

/// Every tool input rejects unknown fields: a typo'd or misplaced argument must be
/// a loud invalid-params error, never silently dropped into the configured
/// defaults (the caller would believe the override applied — a textbook silent
/// fallback, the same hazard the retired `provider` alias once closed).
#[test]
fn a_typoed_argument_is_a_loud_error_not_a_silent_default_run() {
    // A misspelled override on consult.
    let err = serde_json::from_value::<ConsultInput>(
        json!({ "question": "q", "explorer_modle": "claude-haiku-4-5" }),
    )
    .expect_err("a typo'd consult argument must be rejected");
    assert!(
        err.to_string().contains("explorer_modle"),
        "the error must name the unknown field, got: {err}"
    );

    // Another tool's spelling sent to the wrong tool (`max_turns` is explore's).
    serde_json::from_value::<ConsultInput>(json!({ "question": "q", "max_turns": 5 }))
        .expect_err("explore's max_turns spelling must not silently vanish on consult");

    // And the other three inputs hold the same line.
    serde_json::from_value::<ExploreInput>(json!({ "question": "q", "sesion_id": "s" }))
        .expect_err("explore rejects unknown fields");
    serde_json::from_value::<SynthesizeInput>(json!({ "question": "q", "contxt": "evidence" }))
        .expect_err("synthesize rejects unknown fields");
    serde_json::from_value::<RunKaishInput>(json!({ "script": "ls", "paht": "/tmp" }))
        .expect_err("run_kaish rejects unknown fields");
}

/// The `provider` tombstone: with the transitional alias removed, a client still
/// sending the old spelling lands on `deny_unknown_fields` and gets a loud error
/// naming the field — never a silent fall-through to the default cast.
#[test]
fn a_stale_provider_arg_is_a_loud_unknown_field_on_all_three_tools() {
    let payload = json!({ "question": "q", "provider": "gemini" });

    let c = serde_json::from_value::<ConsultInput>(payload.clone());
    let e = serde_json::from_value::<ExploreInput>(payload.clone());
    let s = serde_json::from_value::<SynthesizeInput>(payload);
    for (tool, res) in [
        ("consult", c.err().map(|e| e.to_string())),
        ("explore", e.err().map(|e| e.to_string())),
        ("synthesize", s.err().map(|e| e.to_string())),
    ] {
        let msg = res.unwrap_or_else(|| {
            panic!("{tool}: a stale `provider` arg must be rejected, not resolved silently")
        });
        assert!(
            msg.contains("provider"),
            "{tool}: the error must name the unknown field, got: {msg}"
        );
    }
}
