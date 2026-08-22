//! Gemini image generation — a media backend whose wire is a *completion* wire.
//!
//! # Why this is its own module and its own provider kind
//!
//! Every other media backend kaibo speaks is an images API: a distinct endpoint that
//! takes a prompt and answers with bytes. Gemini has no such endpoint. Image generation
//! is `models/{model}:generateContent` — the identical call as text — with
//! `generationConfig.responseModalities` naming what should come back, confirmed against
//! the live discovery document (`generativelanguage.googleapis.com/$discovery/rest?version=v1beta`,
//! 2026-08-22).
//!
//! That single fact drives the whole design:
//!
//! - **It is a separate `ProviderKind` from `Gemini`**, the way `openai-images` is
//!   separate from `openai`. One vendor, two uses of one endpoint, and a cast slot has
//!   to know which it points at — a reasoning slot aimed here would resolve a completion
//!   model that answers in pictures.
//! - **It answers with words as well as bytes.** A `generateContent` response is a list
//!   of parts, and a model that generated an image frequently says something about it in
//!   the same breath. Those words ride [`crate::media::MediaOutcome::Complete::note`]
//!   rather than being dropped — they are often the only account of what the model
//!   actually did, and on a refusal they are the *only* thing that comes back.
//! - **The TTS door is the same field.** `responseModalities` is an enum of
//!   `TEXT`/`IMAGE`/`AUDIO`, and `generationConfig` carries `speechConfig` beside
//!   `imageConfig`. A speech model is this module's shape with a different modality and a
//!   different config block, which is why the note channel and the part-walking below are
//!   written for parts in general rather than for images specifically.
//!
//! # `TEXT` is requested alongside `IMAGE`, deliberately
//!
//! The spec says `responseModalities` is "an exact match to the modalities of the
//! response" and that a request outside a model's supported combinations is an error.
//! Image-only is not universally supported, so the default asks for `["TEXT", "IMAGE"]`
//! — the combination that works everywhere — and kaibo carries the text rather than
//! suppressing it. An operator who knows their model accepts image-only can say so
//! through `fields`.
//!
//! # No operation vocabulary
//!
//! There are no named operations here: "edit this image" is prose in the prompt, with the
//! image itself as an input part. So [`crate::media::MediaModel::accepts_ops`] stays at
//! its `false` default and a caller passing `op` is refused by the arm — an `op` this
//! provider silently ignored would run a plain generation and return something unrelated.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::media::{MediaArtifact, MediaJobId, MediaOutcome, MediaPollOutcome, MediaRequest};

/// The default base URL for Google's public endpoint. A root, not a route — the client
/// appends its own path, the contract every configurable base URL in kaibo follows.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// The modalities asked for when the caller does not say. See the module doc for why
/// `TEXT` rides along rather than being suppressed.
const DEFAULT_MODALITIES: &[&str] = &["TEXT", "IMAGE"];

/// The `fields` names kaibo routes into `generationConfig.imageConfig` rather than
/// leaving at the top of `generationConfig`.
///
/// A table so the mapping is one list rather than scattered `if`s, and so a caller
/// reading the tool description and this module cannot disagree about where a knob
/// lands. Anything not named here rides `generationConfig` verbatim — the same
/// passthrough posture `fields` has everywhere else in kaibo.
const IMAGE_CONFIG_FIELDS: &[(&str, &str)] = &[
    ("aspect_ratio", "aspectRatio"),
    ("aspectRatio", "aspectRatio"),
    ("image_size", "imageSize"),
    ("imageSize", "imageSize"),
];

/// Why a Gemini image call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeminiImagesError {
    #[error("gemini-images transport failure: {0}")]
    Transport(String),

    #[error("gemini-images returned HTTP {status}: {body}")]
    Provider { status: u16, body: String },

    #[error("gemini-images returned a body that is not JSON: {0}")]
    InvalidBody(String),

    /// The response parsed but carried no candidate at all.
    #[error(
        "gemini-images returned no candidates, so there is nothing to store. This is the \
         provider answering with an empty result rather than an error; retry, or try a \
         different prompt."
    )]
    NoCandidates,

    /// Parts came back, but none of them were image data.
    ///
    /// The most valuable error this module has, and the reason the model's own words are
    /// carried into it: a safety refusal, a clarifying question, and "I cannot do that"
    /// all arrive exactly this way — a normal `200` with text and no picture. Reporting
    /// "no image" without the text would throw away the only explanation the caller gets.
    #[error(
        "gemini-images returned no image, only text. The model said: {said:?}. That is \
         usually a refusal or a request for clarification rather than a fault — read what \
         it said, then adjust the prompt or the input image."
    )]
    NoImageParts { said: String },

    #[error("gemini-images returned an image part whose data is not valid base64: {0}")]
    InvalidB64(String),

    #[error(
        "gemini-images returned an image part with no `mimeType`, so kaibo cannot say what \
         the bytes are and refuses to store them under a guessed type."
    )]
    MissingMime,
}

/// Build the `generateContent` request body for one media request.
///
/// Pure, so the wire shape is testable without a socket. The prompt leads the parts list
/// and every input image follows as an `inlineData` blob, which is the order a
/// conversational model reads: instruction first, material after.
pub fn build_request_body(request: &MediaRequest) -> Value {
    let mut parts: Vec<Value> = vec![json!({ "text": request.prompt })];
    for input in &request.inputs {
        parts.push(json!({
            "inlineData": {
                "mimeType": input.mime,
                "data": base64::engine::general_purpose::STANDARD.encode(&input.bytes),
            }
        }));
    }

    let mut generation_config = Map::new();
    let mut image_config = Map::new();
    for (name, value) in &request.fields {
        match IMAGE_CONFIG_FIELDS
            .iter()
            .find(|(neutral, _)| neutral == name)
        {
            Some((_, wire)) => {
                image_config.insert((*wire).to_string(), value.to_json());
            }
            None => {
                generation_config.insert(name.clone(), value.to_json());
            }
        }
    }
    if !image_config.is_empty() {
        generation_config.insert("imageConfig".to_string(), Value::Object(image_config));
    }
    // Seeded, not forced: a caller that named `responseModalities` in `fields` has already
    // landed it above and keeps it.
    generation_config
        .entry("responseModalities".to_string())
        .or_insert_with(|| json!(DEFAULT_MODALITIES));

    json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": Value::Object(generation_config),
    })
}

/// Walk one `generateContent` response into artifacts plus whatever the model said.
///
/// Written for parts in general rather than images in particular: an `inlineData` part is
/// an artifact whatever its mime, so the day a speech model rides this shape, the walk
/// does not change.
pub fn parse_response(status: u16, body: &[u8]) -> Result<MediaOutcome, GeminiImagesError> {
    if !(200..300).contains(&status) {
        return Err(GeminiImagesError::Provider {
            status,
            body: String::from_utf8_lossy(body).trim().to_string(),
        });
    }
    let parsed: Value =
        serde_json::from_slice(body).map_err(|e| GeminiImagesError::InvalidBody(e.to_string()))?;
    let candidate = parsed
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or(GeminiImagesError::NoCandidates)?;
    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .ok_or(GeminiImagesError::NoCandidates)?;

    let mut artifacts = Vec::new();
    let mut said: Vec<String> = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.trim().is_empty() {
                said.push(text.trim().to_string());
            }
            continue;
        }
        // Both spellings appear in the wild: the REST JSON uses `inlineData`, the proto
        // field name is `inline_data`, and a proxy that round-trips through the proto
        // shape emits the second. Accepting both costs one line and refusing one of them
        // would be a silent empty result.
        let Some(blob) = part.get("inlineData").or_else(|| part.get("inline_data")) else {
            continue;
        };
        let mime = blob
            .get("mimeType")
            .or_else(|| blob.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or(GeminiImagesError::MissingMime)?;
        let data = blob.get("data").and_then(Value::as_str).unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| GeminiImagesError::InvalidB64(e.to_string()))?;
        artifacts.push(MediaArtifact {
            bytes,
            mime: mime.to_string(),
            // `generationConfig.seed` is an input knob; the response does not echo one.
            seed: None,
        });
    }

    let note = (!said.is_empty()).then(|| said.join("\n\n"));
    if artifacts.is_empty() {
        return Err(GeminiImagesError::NoImageParts {
            said: note.unwrap_or_else(|| "(nothing)".to_string()),
        });
    }
    Ok(MediaOutcome::Complete { artifacts, note })
}

/// The HTTP client for one `gemini-images` backend.
#[derive(Clone)]
pub struct GeminiImagesClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl GeminiImagesClient {
    pub fn new(api_key: &str, base_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            http: crate::tls::https_client(timeout)?,
            api_key: api_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// `POST {base}/v1beta/models/{model}:generateContent`.
    ///
    /// The key rides the `x-goog-api-key` header rather than a `?key=` query parameter:
    /// both are accepted, and a header keeps the credential out of anything that logs a
    /// URL.
    pub async fn generate(
        &self,
        model: &str,
        request: &MediaRequest,
    ) -> Result<MediaOutcome, GeminiImagesError> {
        let url = format!("{}/v1beta/models/{}:generateContent", self.base_url, model);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&build_request_body(request))
            .send()
            .await
            .map_err(|e| GeminiImagesError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| GeminiImagesError::Transport(e.to_string()))?;
        parse_response(status, &bytes)
    }
}

/// The [`crate::media::MediaModel`] behind an `image = "backend/model-id"` slot on a
/// `gemini-images` backend.
#[derive(Clone)]
pub struct GeminiImagesModel {
    client: GeminiImagesClient,
    model: String,
}

impl GeminiImagesModel {
    pub fn from_parts(client: &GeminiImagesClient, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::media::MediaModel for GeminiImagesModel {
    /// An input image is how you ask for an edit here — there is no edit *route*, only an
    /// instruction and the material it refers to.
    fn accepts_inputs(&self) -> bool {
        true
    }

    async fn generate(&self, request: &MediaRequest) -> anyhow::Result<MediaOutcome> {
        Ok(self.client.generate(&self.model, request).await?)
    }

    /// `generateContent` is synchronous, so no caller ever holds a job id for this
    /// backend. Bails rather than pretending — a poll arriving here means a job id was
    /// invented or a future change broke the sync-only declaration.
    async fn poll(&self, _job: &MediaJobId) -> anyhow::Result<MediaPollOutcome> {
        anyhow::bail!(
            "gemini-images declares no deferred operations — every generation completes \
             in-call, so there is no provider job to poll"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::Extension;
    use crate::media::{FieldValue, MediaInput};

    fn request(prompt: &str) -> MediaRequest {
        MediaRequest {
            prompt: prompt.to_string(),
            fields: Vec::new(),
            inputs: Vec::new(),
            op: None,
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// The prompt leads and every input image follows as an `inlineData` blob carrying
    /// the store's own mime — instruction first, material after, which is the order a
    /// conversational model reads.
    #[test]
    fn the_prompt_leads_and_inputs_follow_as_inline_blobs() {
        let mut r = request("put a hat on the cat");
        r.inputs = vec![MediaInput::new(
            "image",
            Extension::Png,
            b"\x89PNG\r\n\x1a\ncat".to_vec(),
        )];
        let body = build_request_body(&r);
        let parts = body["contents"][0]["parts"].as_array().expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "put a hat on the cat");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(
            parts[1]["inlineData"]["data"],
            b64(b"\x89PNG\r\n\x1a\ncat"),
            "the bytes ride base64 in the blob"
        );
    }

    /// `TEXT` is asked for beside `IMAGE` by default — image-only is not a combination
    /// every model supports, and the spec makes a mismatched request an error rather
    /// than a downgrade.
    #[test]
    fn text_is_requested_alongside_image_by_default() {
        let body = build_request_body(&request("a lighthouse"));
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            serde_json::json!(["TEXT", "IMAGE"])
        );
    }

    /// Seeded, not forced. An operator who knows their model takes image-only says so
    /// through `fields` and keeps it — kaibo does not overwrite a stated choice.
    #[test]
    fn a_caller_stated_modality_survives_the_default() {
        let mut r = request("a lighthouse");
        r.fields = vec![(
            "responseModalities".to_string(),
            FieldValue::Str("IMAGE".to_string()),
        )];
        let body = build_request_body(&r);
        assert_eq!(body["generationConfig"]["responseModalities"], "IMAGE");
    }

    /// Image knobs land in `imageConfig`; everything else stays at the top of
    /// `generationConfig`, the passthrough posture `fields` has everywhere in kaibo.
    #[test]
    fn image_knobs_route_into_image_config_and_the_rest_passes_through() {
        let mut r = request("a lighthouse");
        r.fields = vec![
            ("aspect_ratio".to_string(), FieldValue::Str("16:9".into())),
            (
                "seed".to_string(),
                FieldValue::Num(serde_json::Number::from(42)),
            ),
        ];
        let body = build_request_body(&r);
        assert_eq!(
            body["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        assert_eq!(body["generationConfig"]["seed"], 42);
        assert!(
            body["generationConfig"]["aspect_ratio"].is_null(),
            "a routed knob does not also stay at the top level"
        );
    }

    /// Images and text arrive interleaved in one response. Both are kept: the bytes
    /// become artifacts, the words become the note.
    #[test]
    fn text_and_image_parts_both_survive_the_walk() {
        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"text": "I moved the sign left; the original crop cut it off."},
                {"inlineData": {"mimeType": "image/png", "data": b64(b"the-image")}}
            ]}}]
        });
        let outcome = parse_response(200, body.to_string().as_bytes()).expect("parses");
        let MediaOutcome::Complete { artifacts, note } = outcome else {
            panic!("generateContent is synchronous")
        };
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].bytes, b"the-image");
        assert_eq!(artifacts[0].mime, "image/png");
        assert_eq!(
            note.as_deref(),
            Some("I moved the sign left; the original crop cut it off."),
            "the model's account of what it did is not dropped"
        );
    }

    /// **The refusal path, and the reason the note exists at all.** A safety refusal is
    /// a normal `200` with text and no picture. Reporting "no image" without the text
    /// would throw away the only explanation the caller gets.
    #[test]
    fn text_with_no_image_is_an_error_that_carries_what_the_model_said() {
        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"text": "I can't create an image of a real person."}
            ]}}]
        });
        let err = parse_response(200, body.to_string().as_bytes()).expect_err("no image");
        let msg = err.to_string();
        assert!(
            msg.contains("I can't create an image of a real person."),
            "the refusal carries the model's own words: {msg}"
        );
        assert!(
            msg.contains("usually a refusal"),
            "and says what it means: {msg}"
        );
    }

    /// The proto spelling round-trips through some proxies. Accepting both costs a line;
    /// refusing one would be a silently empty result.
    #[test]
    fn the_snake_case_blob_spelling_is_also_accepted() {
        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"inline_data": {"mime_type": "image/jpeg", "data": b64(b"jpg")}}
            ]}}]
        });
        let MediaOutcome::Complete { artifacts, .. } =
            parse_response(200, body.to_string().as_bytes()).expect("parses")
        else {
            panic!("synchronous")
        };
        assert_eq!(artifacts[0].mime, "image/jpeg");
    }

    /// A blob with no mime is refused rather than stored under a guess — the same rule
    /// `write_cas` applies from the other direction.
    #[test]
    fn a_blob_without_a_mime_is_refused_rather_than_guessed() {
        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"inlineData": {"data": b64(b"bytes")}}
            ]}}]
        });
        assert!(matches!(
            parse_response(200, body.to_string().as_bytes()),
            Err(GeminiImagesError::MissingMime)
        ));
    }

    #[test]
    fn a_non_2xx_carries_the_providers_own_body() {
        let err = parse_response(429, br#"{"error":{"message":"quota"}}"#).expect_err("429");
        let msg = err.to_string();
        assert!(msg.contains("429") && msg.contains("quota"), "{msg}");
    }

    #[test]
    fn an_empty_candidate_list_is_refused_clearly() {
        let err = parse_response(200, br#"{"candidates":[]}"#).expect_err("no candidates");
        assert!(matches!(err, GeminiImagesError::NoCandidates));
    }
}
