//! Canary on rig's Anthropic *wire* serialization — the one hop kaibo's offline
//! harness can't see. The scripted transport in `test_support.rs` drives the real
//! consult loop but never serializes a request to Anthropic's JSON, so a provider-side
//! encoding bug passes every offline test and still 400s live.
//!
//! That exact gap bit us on 2026-06-12: `view_image` was green offline yet every
//! Anthropic call failed, because rig-core 0.34 modeled a tool-result image as a
//! newtype variant under `#[serde(tag = "type")]`. Wrapping an internally-tagged enum
//! in an internally-tagged newtype makes serde emit a *duplicate* `type` key —
//! `{"type":"image","type":"base64",…}` — and Anthropic keeps the last, sees
//! `type:"base64"`, rejects the block.
//!
//! rig 0.46 no longer exposes Anthropic's wire types, so this test goes one level up:
//! it hands a real Messages model a history whose tool result carries an image, and
//! reads the request body that would have left the process. An image tool result must
//! serialize to a single `type:"image"` with a nested `source`, never a top-level
//! `base64` tag. It fails the moment a rig bump regresses the wire shape, where our mock
//! would stay green.

use rig_core::completion::message::{
    AssistantContent, CallId, DocumentSourceKind, Image, ImageMediaType, Message, MimeType, ToolName,
    ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::providers::anthropic::AnthropicConfig;
use rig_core::test_utils::RecordingHttpClient;

#[tokio::test]
async fn anthropic_tool_result_image_nests_its_source() {
    let http = RecordingHttpClient::with_error(http::StatusCode::IM_A_TEAPOT, "capture only");
    let model = AnthropicConfig::new("test-key")
        .connect(http.clone())
        .completion("claude-sonnet-5-5");

    let call = CallId::from_wire("toolu_1");
    let name = ToolName::new("view_image").expect("tool name");
    let request = CompletionRequest::new(Message::User {
        content: vec![UserContent::ToolResult(ToolResult {
            call: call.clone(),
            name: name.clone(),
            content: vec![
                ToolResultContent::text("Loaded image shot.png."),
                ToolResultContent::Image(Image {
                    data: DocumentSourceKind::Base64("QUJD".to_string()),
                    media_type: ImageMediaType::from_mime_type("image/png"),
                    ..Image::default()
                }),
            ],
            is_error: false,
        })],
    })
    .message(Message::user("look at shot.png"))
    .message(Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![AssistantContent::tool_call(
            "toolu_1",
            name,
            serde_json::json!({ "path": "shot.png" }),
        )]),
    ))
    // A request that declares no tools has its tool calls and results rewritten as plain
    // text; a real loop always declares its tools, so this one does too.
    .tool(ToolDefinition::new(
        ToolName::new("view_image").expect("tool name"),
        "look at an image",
        serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
    ))
    .max_tokens(1024);
    // The capture transport always errors; the body is already built by then.
    let _ = model.call(request).await;

    let sent = http.requests();
    let body: serde_json::Value =
        serde_json::from_slice(&sent.first().expect("a request was sent").body)
            .expect("the request body is JSON");
    let block = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result")
        .unwrap_or_else(|| panic!("a tool_result block rides the request: {body}"));
    let img = block["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["type"] == "image")
        .unwrap_or_else(|| panic!("tool-result image must be tagged type:image: {block}"));

    // The base64 tag belongs to the NESTED source, never the top level — a top-level
    // base64 is the duplicate-key bug that reached Anthropic as the wire type and 400'd.
    let source = img
        .get("source")
        .unwrap_or_else(|| panic!("image block must carry a nested source, got {img}"));
    assert_eq!(
        source.get("type").and_then(|t| t.as_str()),
        Some("base64"),
        "the base64 tag lives on source, got {img}"
    );
    assert_eq!(
        source.get("media_type").and_then(|m| m.as_str()),
        Some("image/png")
    );
    assert_eq!(source.get("data").and_then(|d| d.as_str()), Some("QUJD"));
}
