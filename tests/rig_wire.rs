//! Canary on rig's Anthropic *wire* serialization — the one hop kaibo's offline
//! harness can't see. The scripted `CompletionClient` in `test_support.rs` drives
//! the real consult loop but never serializes a request to Anthropic's JSON, so a
//! provider-side encoding bug passes every offline test and still 400s live.
//!
//! That exact gap bit us on 2026-06-12: `view_image` was green offline yet every
//! Anthropic call failed, because rig-core 0.34 modeled a tool-result image as a
//! newtype variant `ToolResultContent::Image(ImageSource)` under `#[serde(tag =
//! "type")]`. Wrapping an internally-tagged enum in an internally-tagged newtype
//! makes serde emit a *duplicate* `type` key — `{"type":"image","type":"base64",…}`
//! — and Anthropic keeps the last, sees `type:"base64"`, rejects the block. The
//! 0.38 fix makes it a struct variant `Image { source }`, nesting correctly.
//!
//! This test pins that contract directly on rig's own type: an image tool result
//! must serialize to a single `type:"image"` with a nested `source`, never a
//! top-level `base64` tag. It fails the moment a rig bump (or downgrade) regresses
//! the wire shape, where our mock would stay green.

use rig_core::providers::anthropic::completion::{ImageFormat, ImageSource, ToolResultContent};

#[test]
fn anthropic_tool_result_image_nests_its_source() {
    let img = ToolResultContent::Image {
        source: ImageSource::Base64 {
            data: "QUJD".to_string(),
            media_type: ImageFormat::PNG,
        },
    };
    let v: serde_json::Value = serde_json::to_value(&img).unwrap();

    // The content block is an image, not a bare source.
    assert_eq!(
        v.get("type").and_then(|t| t.as_str()),
        Some("image"),
        "tool-result image must be tagged type:image, got {v}"
    );
    // The base64 tag belongs to the NESTED source, never the top level — a
    // top-level base64 is the duplicate-key bug that reached Anthropic as the wire
    // type and 400'd.
    let source = v
        .get("source")
        .unwrap_or_else(|| panic!("image block must carry a nested source, got {v}"));
    assert_eq!(
        source.get("type").and_then(|t| t.as_str()),
        Some("base64"),
        "the base64 tag lives on source, got {v}"
    );
    assert_eq!(
        source.get("media_type").and_then(|m| m.as_str()),
        Some("image/png")
    );
    assert_eq!(source.get("data").and_then(|d| d.as_str()), Some("QUJD"));
}
