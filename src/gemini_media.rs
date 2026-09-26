//! Gemini image and speech generation — a media backend whose wire is a *completion*
//! wire.
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
//! - **Speech is the same call.** `responseModalities` is an enum of
//!   `TEXT`/`IMAGE`/`AUDIO`, and `generationConfig` carries `speechConfig` beside
//!   `imageConfig`. A model built for an `audio` slot ([`MediaOutput::Audio`]) asks for
//!   `AUDIO` alone and routes `voice`/`speakers`/`language_code` into `speechConfig`; one
//!   built for an `image` slot asks for `TEXT`+`IMAGE` and routes the image knobs. The
//!   slot decides, never a sniff of the model id.
//!
//! # Speech arrives as raw PCM, and is stored as WAVE
//!
//! The speech models answer in one of two shapes, both seen live on 2026-09-26: the 3.8
//! family sends a finished `audio/wav`, stored as it came; `gemini-2.5-*-preview-tts`
//! sends bare 16-bit samples (`audio/L16;codec=pcm;rate=24000`) with no container.
//! [`crate::wav`] wraps the bare samples before they leave this module, so the artifact
//! is an ordinary `audio/wav` any player opens, and the rate lives in the file rather
//! than in a mime parameter the CAS does not keep. Several PCM parts in one candidate are
//! one utterance in pieces and are joined into one file. The samples are little-endian:
//! RFC 2586 defines `L16` as big-endian, but the first samples of a live response
//! (`fdff feff 0000 fdff`) are near-silence only when read little-endian, and Google's
//! own examples write the bytes straight into a WAVE file.
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

use crate::media::{
    FieldValue, MediaArtifact, MediaJobId, MediaOutcome, MediaOutput, MediaPollOutcome,
    MediaRequest,
};

/// The default base URL for Google's public endpoint. A root, not a route — the client
/// appends its own path, the contract every configurable base URL in kaibo follows.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// The modalities an image slot asks for when the caller does not say. See the module
/// doc for why `TEXT` rides along rather than being suppressed.
const IMAGE_MODALITIES: &[&str] = &["TEXT", "IMAGE"];

/// The modalities an audio slot asks for. `AUDIO` alone: the speech models reject a
/// request that also asks for `TEXT`.
const AUDIO_MODALITIES: &[&str] = &["AUDIO"];

/// The mime a stored speech artifact carries once its PCM is wrapped.
const WAV_MIME: &str = "audio/wav";

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
pub enum GeminiMediaError {
    #[error("gemini-media transport failure: {0}")]
    Transport(String),

    #[error("gemini-media returned HTTP {status}: {body}")]
    Provider { status: u16, body: String },

    #[error("gemini-media returned a body that is not JSON: {0}")]
    InvalidBody(String),

    /// The response parsed but carried no candidate at all.
    #[error(
        "gemini-media returned no candidates, so there is nothing to store. This is the \
         provider answering with an empty result rather than an error; retry, or try a \
         different prompt."
    )]
    NoCandidates,

    /// The model stopped without producing content, and said why.
    ///
    /// Distinct from [`GeminiMediaError::NoCandidates`] because the two are opposite
    /// situations that a single message would describe wrongly: this one *has* a
    /// candidate, or a prompt-level verdict, and the reason it carries is the whole
    /// value of the error. Reporting "no candidates" here would be false and would throw
    /// away the one field that says what to change.
    #[error(
        "gemini-media produced no content: {reason}. That is the model declining or \
         stopping rather than a fault — the reason names which policy or limit it hit, so \
         change the prompt or the input image to suit it."
    )]
    Blocked { reason: String },

    /// Parts came back, but none of them were image or audio data.
    ///
    /// The most valuable error this module has, and the reason the model's own words are
    /// carried into it: a safety refusal, a clarifying question, and "I cannot do that"
    /// all arrive exactly this way — a normal `200` with text and no artifact. Reporting
    /// "no image" without the text would throw away the only explanation the caller gets.
    #[error(
        "gemini-media returned no {expected}, only text. The model said: {said:?}. That is \
         usually a refusal or a request for clarification rather than a fault — read what \
         it said, then adjust the prompt."
    )]
    NoArtifact {
        expected: &'static str,
        said: String,
    },

    #[error("gemini-media returned a data part that is not valid base64: {0}")]
    InvalidB64(String),

    #[error(
        "gemini-media returned a data part with no `mimeType`, so kaibo cannot say what \
         the bytes are and refuses to store them under a guessed type."
    )]
    MissingMime,

    /// A `fields` entry this module reads for itself has the wrong type or shape. Refused
    /// before a request is sent, naming the shape that works.
    #[error("{0}")]
    InvalidField(String),

    /// Raw PCM that cannot become a playable file: no rate, a partial sample, or pieces
    /// at different rates.
    #[error("gemini-media returned speech kaibo cannot store as audio: {0}")]
    Pcm(String),
}

/// Build the `generateContent` request body for one media request.
///
/// Pure, so the wire shape is testable without a socket. The prompt leads the parts list
/// and every input image follows as an `inlineData` blob, which is the order a
/// conversational model reads: instruction first, material after. `output` is the slot
/// this model staffs: it picks the default modalities and which knobs `fields` routes
/// into a nested config block.
pub fn build_request_body(
    request: &MediaRequest,
    output: MediaOutput,
) -> Result<Value, GeminiMediaError> {
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
    match output {
        MediaOutput::Image => {
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
        }
        MediaOutput::Audio => {
            if let Some(speech) = speech_config(&request.fields)? {
                generation_config.insert("speechConfig".to_string(), speech);
            }
            for (name, value) in &request.fields {
                if !SPEECH_FIELDS.contains(&name.as_str()) {
                    generation_config.insert(name.clone(), value.to_json());
                }
            }
        }
    }
    // Seeded, not forced: a caller that named `responseModalities` in `fields` has already
    // landed it above and keeps it.
    let modalities = match output {
        MediaOutput::Image => IMAGE_MODALITIES,
        MediaOutput::Audio => AUDIO_MODALITIES,
    };
    generation_config
        .entry("responseModalities".to_string())
        .or_insert_with(|| json!(modalities));

    Ok(json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": Value::Object(generation_config),
    }))
}

/// The `fields` names an audio slot reads into `speechConfig` instead of passing through.
const SPEECH_FIELDS: &[&str] = &["voice", "speakers", "language_code", "languageCode"];

/// Build `speechConfig` from the speech fields, or `None` when the caller named none (the
/// model then speaks in its default voice).
///
/// `voice` is one prebuilt voice (`"Kore"`). `speakers` is `"Joe=Kore, Jane=Puck"`: each
/// name the prompt's dialogue uses, paired with the voice that reads it. The two are
/// different answers to the same question, so naming both is refused rather than left for
/// the provider to resolve silently.
fn speech_config(fields: &[(String, FieldValue)]) -> Result<Option<Value>, GeminiMediaError> {
    let text_field = |name: &str| -> Result<Option<&str>, GeminiMediaError> {
        match fields.iter().find(|(n, _)| n == name) {
            None => Ok(None),
            Some((_, value)) => value.as_str().map(Some).ok_or_else(|| {
                GeminiMediaError::InvalidField(format!(
                    "`fields.{name}` must be a string, got {}. Nothing was sent.",
                    value.to_json()
                ))
            }),
        }
    };
    let voice = text_field("voice")?;
    let speakers = text_field("speakers")?;
    let language = match text_field("language_code")? {
        Some(code) => Some(code),
        None => text_field("languageCode")?,
    };

    let prebuilt = |name: &str| json!({ "prebuiltVoiceConfig": { "voiceName": name.trim() } });
    let mut speech = Map::new();
    match (voice, speakers) {
        (Some(_), Some(_)) => {
            return Err(GeminiMediaError::InvalidField(
                "`fields.voice` and `fields.speakers` were both given. Use `voice` for one \
                 speaker, or `speakers` (\"Joe=Kore, Jane=Puck\") for a dialogue — not both. \
                 Nothing was sent."
                    .to_string(),
            ));
        }
        (Some(voice), None) => {
            speech.insert("voiceConfig".to_string(), prebuilt(voice));
        }
        (None, Some(speakers)) => {
            let mut configs = Vec::new();
            for entry in speakers.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                let Some((name, voice)) = entry
                    .split_once('=')
                    .filter(|(n, v)| !n.trim().is_empty() && !v.trim().is_empty())
                else {
                    return Err(GeminiMediaError::InvalidField(format!(
                        "`fields.speakers` entry {entry:?} is not `Name=Voice`. Write every \
                         speaker as `Name=Voice`, separated by commas: \"Joe=Kore, \
                         Jane=Puck\". Nothing was sent."
                    )));
                };
                configs.push(json!({ "speaker": name.trim(), "voiceConfig": prebuilt(voice) }));
            }
            if configs.is_empty() {
                return Err(GeminiMediaError::InvalidField(
                    "`fields.speakers` names no speaker. Write every speaker as `Name=Voice`, \
                     separated by commas: \"Joe=Kore, Jane=Puck\", or use `voice` for one \
                     speaker. Nothing was sent."
                        .to_string(),
                ));
            }
            speech.insert(
                "multiSpeakerVoiceConfig".to_string(),
                json!({ "speakerVoiceConfigs": configs }),
            );
        }
        (None, None) => {}
    }
    if let Some(code) = language {
        speech.insert("languageCode".to_string(), json!(code.trim()));
    }
    Ok((!speech.is_empty()).then_some(Value::Object(speech)))
}

/// Walk one candidate's parts into artifacts and commentary.
///
/// Split out so every candidate goes through the same walk, and written for parts in
/// general: an `inlineData` part is an artifact whatever its mime, so the day a speech
/// model rides this shape the walk does not change.
fn walk_parts(
    parts: &[Value],
    artifacts: &mut Vec<MediaArtifact>,
    said: &mut Vec<String>,
    unknown: &mut std::collections::BTreeSet<String>,
) -> Result<(), GeminiMediaError> {
    // Raw PCM pieces of this candidate, joined into one WAVE file after the walk: one
    // candidate is one utterance, however many parts it arrives in.
    let mut pcm: Option<(crate::wav::PcmFormat, Vec<u8>)> = None;
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
            // Not text, not a blob. Recorded rather than dropped: if nothing usable comes
            // back, the caller is told which shapes it *did* get, which is the difference
            // between a debuggable response and "(nothing)".
            if let Some(obj) = part.as_object() {
                unknown.extend(obj.keys().cloned());
            }
            continue;
        };
        let mime = blob
            .get("mimeType")
            .or_else(|| blob.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or(GeminiMediaError::MissingMime)?;
        let data = blob.get("data").and_then(Value::as_str).unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| GeminiMediaError::InvalidB64(e.to_string()))?;
        if let Some(format) = crate::wav::parse_raw_pcm_mime(mime) {
            let format = format.map_err(GeminiMediaError::Pcm)?;
            match &mut pcm {
                None => pcm = Some((format, bytes)),
                Some((first, joined)) if *first == format => joined.extend_from_slice(&bytes),
                Some((first, _)) => {
                    return Err(GeminiMediaError::Pcm(format!(
                        "one candidate's audio arrived in pieces with different formats \
                         ({} Hz × {} and {} Hz × {} channel(s)), which cannot share one \
                         file at the right rate",
                        first.sample_rate, first.channels, format.sample_rate, format.channels
                    )));
                }
            }
            continue;
        }
        artifacts.push(MediaArtifact {
            bytes,
            mime: mime.to_string(),
            // `generationConfig.seed` is an input knob; the response does not echo one.
            seed: None,
        });
    }
    if let Some((format, samples)) = pcm {
        let bytes = crate::wav::wrap_pcm16(&samples, format)
            .map_err(|e| GeminiMediaError::Pcm(e.to_string()))?;
        artifacts.push(MediaArtifact {
            bytes,
            mime: WAV_MIME.to_string(),
            seed: None,
        });
    }
    Ok(())
}

/// Walk one `generateContent` response into artifacts plus whatever the model said.
///
/// Written for parts in general rather than images in particular: an `inlineData` part is
/// an artifact whatever its mime, and raw PCM is wrapped as WAVE on the way. `output` only
/// names what was expected when nothing came back; which family the artifacts belong to
/// is checked where they are stored.
pub fn parse_response(
    status: u16,
    body: &[u8],
    output: MediaOutput,
) -> Result<MediaOutcome, GeminiMediaError> {
    if !(200..300).contains(&status) {
        return Err(GeminiMediaError::Provider {
            status,
            body: String::from_utf8_lossy(body).trim().to_string(),
        });
    }
    let parsed: Value =
        serde_json::from_slice(body).map_err(|e| GeminiMediaError::InvalidBody(e.to_string()))?;
    // A prompt-level refusal has no candidates at all and puts its verdict here, so it
    // is read before the candidate list is even looked for.
    if let Some(block) = parsed
        .get("promptFeedback")
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(GeminiMediaError::Blocked {
            reason: format!("the prompt was blocked ({block})"),
        });
    }
    let candidates = parsed
        .get("candidates")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .ok_or(GeminiMediaError::NoCandidates)?;

    let mut artifacts = Vec::new();
    let mut said: Vec<String> = Vec::new();
    // Every candidate, not just the first. `candidateCount` rides `fields` straight into
    // `generationConfig`, so a caller can ask for several — and each one is paid for.
    // Keeping only `.first()` would drop artifacts the caller was billed for, silently.
    let mut unknown_parts: std::collections::BTreeSet<String> = Default::default();
    for candidate in candidates {
        let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        else {
            // A candidate with no content stopped for a reason it names. Only fatal if
            // no other candidate produced anything, which the emptiness check below
            // decides — one blocked candidate among several is not the whole answer.
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                said.push(format!("(stopped: {reason})"));
            }
            continue;
        };
        walk_parts(parts, &mut artifacts, &mut said, &mut unknown_parts)?;
    }
    let note = (!said.is_empty()).then(|| said.join("\n\n"));
    if artifacts.is_empty() {
        return Err(GeminiMediaError::NoArtifact {
            expected: output.key(),
            said: note.unwrap_or_else(|| {
                if unknown_parts.is_empty() {
                    "(nothing)".to_string()
                } else {
                    // Naming the shapes that did arrive is what makes a malformed
                    // response debuggable instead of a shrug.
                    format!(
                        "(no text; the response carried only these part kinds: {})",
                        unknown_parts.into_iter().collect::<Vec<_>>().join(", ")
                    )
                }
            }),
        });
    }
    Ok(MediaOutcome::Complete { artifacts, note })
}

/// The HTTP client for one `gemini-media` backend.
#[derive(Clone)]
pub struct GeminiMediaClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl GeminiMediaClient {
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
        output: MediaOutput,
    ) -> Result<MediaOutcome, GeminiMediaError> {
        let body = build_request_body(request, output)?;
        let url = format!("{}/v1beta/models/{}:generateContent", self.base_url, model);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| GeminiMediaError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| GeminiMediaError::Transport(e.to_string()))?;
        parse_response(status, &bytes, output)
    }
}

/// The [`crate::media::MediaModel`] behind an `image` or `audio` slot on a
/// `gemini-media` backend. Built for one output; the slot it staffs decides which.
#[derive(Clone)]
pub struct GeminiMediaModel {
    client: GeminiMediaClient,
    model: String,
    output: MediaOutput,
}

impl GeminiMediaModel {
    pub fn from_parts(
        client: &GeminiMediaClient,
        model: impl Into<String>,
        output: MediaOutput,
    ) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
            output,
        }
    }
}

#[async_trait::async_trait]
impl crate::media::MediaModel for GeminiMediaModel {
    /// An input image is how you ask for an image edit here — there is no edit *route*,
    /// only an instruction and the material it refers to. Speech takes no input: the
    /// prompt is the whole script.
    fn accepts_inputs(&self) -> bool {
        self.output == MediaOutput::Image
    }

    async fn generate(&self, request: &MediaRequest) -> anyhow::Result<MediaOutcome> {
        Ok(self
            .client
            .generate(&self.model, request, self.output)
            .await?)
    }

    /// `generateContent` is synchronous, so no caller ever holds a job id for this
    /// backend. Bails rather than pretending — a poll arriving here means a job id was
    /// invented or a future change broke the sync-only declaration.
    async fn poll(&self, _job: &MediaJobId) -> anyhow::Result<MediaPollOutcome> {
        anyhow::bail!(
            "gemini-media declares no deferred operations — every generation completes \
             in-call, so there is no provider job to poll"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::Extension;
    use crate::media::MediaInput;

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
        let body = build_request_body(&r, MediaOutput::Image).unwrap();
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
        let body = build_request_body(&request("a lighthouse"), MediaOutput::Image).unwrap();
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
        let body = build_request_body(&r, MediaOutput::Image).unwrap();
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
        let body = build_request_body(&r, MediaOutput::Image).unwrap();
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
        let outcome =
            parse_response(200, body.to_string().as_bytes(), MediaOutput::Image).expect("parses");
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
        let err = parse_response(200, body.to_string().as_bytes(), MediaOutput::Image)
            .expect_err("no image");
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
            parse_response(200, body.to_string().as_bytes(), MediaOutput::Image).expect("parses")
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
            parse_response(200, body.to_string().as_bytes(), MediaOutput::Image),
            Err(GeminiMediaError::MissingMime)
        ));
    }

    /// **A prompt-level block is not "no candidates".** Gemini refuses a prompt before
    /// producing anything, with the verdict in `promptFeedback.blockReason` and no
    /// candidate list at all. Reporting "returned no candidates" there would be false
    /// and would throw away the one field that says what to change.
    #[test]
    fn a_blocked_prompt_reports_the_reason_not_an_empty_result() {
        let body = serde_json::json!({ "promptFeedback": { "blockReason": "SAFETY" } });
        let err = parse_response(200, body.to_string().as_bytes(), MediaOutput::Image)
            .expect_err("blocked");
        assert!(matches!(err, GeminiMediaError::Blocked { .. }));
        let msg = err.to_string();
        assert!(msg.contains("SAFETY"), "the verdict survives: {msg}");
        assert!(msg.contains("prompt was blocked"), "{msg}");
    }

    /// A candidate that stopped without content names why in `finishReason`, and that
    /// reason reaches the caller instead of being swallowed as an empty walk.
    #[test]
    fn a_candidate_that_stopped_carries_its_finish_reason() {
        let body = serde_json::json!({
            "candidates": [{ "finishReason": "IMAGE_SAFETY" }]
        });
        let err = parse_response(200, body.to_string().as_bytes(), MediaOutput::Image)
            .expect_err("no image");
        assert!(
            err.to_string().contains("IMAGE_SAFETY"),
            "the finish reason is the whole diagnosis: {err}"
        );
    }

    /// **Every candidate is walked, not just the first.** `candidateCount` rides
    /// `fields` straight into `generationConfig`, so a caller can ask for several — and
    /// each is billed. Keeping only the first would drop paid-for artifacts silently.
    #[test]
    fn artifacts_from_every_candidate_survive() {
        let body = serde_json::json!({
            "candidates": [
                {"content": {"parts": [
                    {"inlineData": {"mimeType": "image/png", "data": b64(b"one")}}
                ]}},
                {"content": {"parts": [
                    {"inlineData": {"mimeType": "image/png", "data": b64(b"two")}}
                ]}}
            ]
        });
        let MediaOutcome::Complete { artifacts, .. } =
            parse_response(200, body.to_string().as_bytes(), MediaOutput::Image).expect("parses")
        else {
            panic!("synchronous")
        };
        assert_eq!(artifacts.len(), 2, "both paid-for images are kept");
        assert_eq!(artifacts[0].bytes, b"one");
        assert_eq!(artifacts[1].bytes, b"two");
    }

    /// When nothing usable comes back, the error names the part kinds that *did* arrive
    /// — the difference between a debuggable response and "(nothing)".
    #[test]
    fn unrecognized_parts_are_named_rather_than_reported_as_nothing() {
        let body = serde_json::json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "whatever"}}
            ]}}]
        });
        let err = parse_response(200, body.to_string().as_bytes(), MediaOutput::Image)
            .expect_err("no image");
        let msg = err.to_string();
        assert!(msg.contains("functionCall"), "names what did arrive: {msg}");
        assert!(!msg.contains("(nothing)"), "and does not shrug: {msg}");
    }

    #[test]
    fn a_non_2xx_carries_the_providers_own_body() {
        let err = parse_response(429, br#"{"error":{"message":"quota"}}"#, MediaOutput::Image)
            .expect_err("429");
        let msg = err.to_string();
        assert!(msg.contains("429") && msg.contains("quota"), "{msg}");
    }

    // --- speech: the audio slot ------------------------------------------------------

    fn speech_body(r: &MediaRequest) -> Value {
        build_request_body(r, MediaOutput::Audio).expect("a valid speech request")
    }

    fn pcm_response(parts: Value) -> Vec<u8> {
        serde_json::json!({ "candidates": [{ "content": { "parts": parts } }] })
            .to_string()
            .into_bytes()
    }

    /// A speech model is asked for `AUDIO` alone — TTS models reject a `TEXT` modality —
    /// and the prompt is the text to speak.
    #[test]
    fn speech_asks_for_audio_only() {
        let body = speech_body(&request("Say cheerfully: have a wonderful day!"));
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            serde_json::json!(["AUDIO"])
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["text"],
            "Say cheerfully: have a wonderful day!"
        );
        assert!(body["generationConfig"]["imageConfig"].is_null());
    }

    /// `voice` lands where the API reads a prebuilt voice, and `language_code` beside it.
    #[test]
    fn voice_and_language_route_into_speech_config() {
        let mut r = request("hello");
        r.fields = vec![
            ("voice".to_string(), FieldValue::Str("Kore".into())),
            ("language_code".to_string(), FieldValue::Str("en-US".into())),
            (
                "temperature".to_string(),
                FieldValue::Num(serde_json::Number::from(1)),
            ),
        ];
        let body = speech_body(&r);
        let speech = &body["generationConfig"]["speechConfig"];
        assert_eq!(
            speech["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Kore"
        );
        assert_eq!(speech["languageCode"], "en-US");
        assert_eq!(
            body["generationConfig"]["temperature"], 1,
            "everything else still passes through"
        );
        assert!(body["generationConfig"]["voice"].is_null());
    }

    /// Two speakers, each with its own voice, from one `Name=Voice, Name=Voice` string —
    /// the names are the ones the prompt's dialogue uses.
    #[test]
    fn speakers_build_a_multi_speaker_config() {
        let mut r = request("Joe: Hi Jane.\nJane: Hi Joe!");
        r.fields = vec![(
            "speakers".to_string(),
            FieldValue::Str("Joe=Kore, Jane=Puck".into()),
        )];
        let body = speech_body(&r);
        let configs = &body["generationConfig"]["speechConfig"]["multiSpeakerVoiceConfig"]
            ["speakerVoiceConfigs"];
        assert_eq!(configs[0]["speaker"], "Joe");
        assert_eq!(
            configs[0]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Kore"
        );
        assert_eq!(configs[1]["speaker"], "Jane");
        assert_eq!(
            configs[1]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Puck"
        );
    }

    /// `voice` and `speakers` are two answers to one question. Sending both would let the
    /// provider pick one silently, so kaibo refuses before spending a request.
    #[test]
    fn voice_and_speakers_together_are_refused() {
        let mut r = request("hello");
        r.fields = vec![
            ("voice".to_string(), FieldValue::Str("Kore".into())),
            ("speakers".to_string(), FieldValue::Str("Joe=Puck".into())),
        ];
        let err = build_request_body(&r, MediaOutput::Audio).expect_err("ambiguous");
        let msg = err.to_string();
        assert!(msg.contains("voice") && msg.contains("speakers"), "{msg}");
    }

    #[test]
    fn a_malformed_speakers_entry_is_refused_with_the_shape() {
        let mut r = request("hello");
        r.fields = vec![("speakers".to_string(), FieldValue::Str("Joe".into()))];
        let err = build_request_body(&r, MediaOutput::Audio).expect_err("no voice");
        assert!(err.to_string().contains("Name=Voice"), "{err}");
    }

    /// An empty `speakers` would send a multi-speaker config with no speakers.
    #[test]
    fn an_empty_speakers_field_is_refused() {
        for empty in ["", " , "] {
            let mut r = request("hello");
            r.fields = vec![("speakers".to_string(), FieldValue::Str(empty.into()))];
            let err = build_request_body(&r, MediaOutput::Audio).expect_err("no speakers");
            assert!(err.to_string().contains("names no speaker"), "{err}");
        }
    }

    /// `voice` is a string. A number there is the caller's mistake to hear about.
    #[test]
    fn a_non_string_voice_is_refused() {
        let mut r = request("hello");
        r.fields = vec![(
            "voice".to_string(),
            FieldValue::Num(serde_json::Number::from(3)),
        )];
        assert!(build_request_body(&r, MediaOutput::Audio).is_err());
    }

    /// **Raw PCM becomes a playable WAVE file.** Gemini answers with bare 16-bit samples
    /// and the rate in a mime parameter; the stored artifact is `audio/wav` with a header
    /// that carries that rate.
    #[test]
    fn raw_pcm_is_wrapped_as_wav() {
        let samples = [1u8, 0, 2, 0, 3, 0, 4, 0];
        let body = pcm_response(serde_json::json!([
            {"inlineData": {"mimeType": "audio/L16;codec=pcm;rate=24000", "data": b64(&samples)}}
        ]));
        let MediaOutcome::Complete { artifacts, note } =
            parse_response(200, &body, MediaOutput::Audio).expect("parses")
        else {
            panic!("synchronous")
        };
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].mime, "audio/wav");
        assert_eq!(&artifacts[0].bytes[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(artifacts[0].bytes[24..28].try_into().unwrap()),
            24_000,
            "the rate from the mime reaches the header"
        );
        assert_eq!(&artifacts[0].bytes[44..], &samples);
        assert_eq!(note, None);
    }

    /// Several PCM parts in one candidate are one utterance in pieces, so they become one
    /// file in order. Storing each piece as its own artifact would hand back fragments.
    #[test]
    fn pcm_parts_of_one_candidate_join_into_one_file() {
        let body = pcm_response(serde_json::json!([
            {"inlineData": {"mimeType": "audio/L16;codec=pcm;rate=24000", "data": b64(&[1, 0])}},
            {"inlineData": {"mimeType": "audio/L16;codec=pcm;rate=24000", "data": b64(&[2, 0])}}
        ]));
        let MediaOutcome::Complete { artifacts, .. } =
            parse_response(200, &body, MediaOutput::Audio).expect("parses")
        else {
            panic!("synchronous")
        };
        assert_eq!(artifacts.len(), 1);
        assert_eq!(&artifacts[0].bytes[44..], &[1, 0, 2, 0]);
    }

    /// Pieces at different rates cannot share one header; joining them would play part
    /// of the audio at the wrong pitch, so the response is refused.
    #[test]
    fn pcm_parts_at_different_rates_are_refused() {
        let body = pcm_response(serde_json::json!([
            {"inlineData": {"mimeType": "audio/L16;rate=24000", "data": b64(&[1, 0])}},
            {"inlineData": {"mimeType": "audio/L16;rate=16000", "data": b64(&[2, 0])}}
        ]));
        let err = parse_response(200, &body, MediaOutput::Audio).expect_err("mixed rates");
        assert!(err.to_string().contains("rate"), "{err}");
    }

    /// An odd byte count is a cut or mislabelled stream; it is refused, not padded.
    #[test]
    fn a_truncated_pcm_stream_is_refused() {
        let body = pcm_response(serde_json::json!([
            {"inlineData": {"mimeType": "audio/L16;rate=24000", "data": b64(&[1, 0, 2])}}
        ]));
        assert!(parse_response(200, &body, MediaOutput::Audio).is_err());
    }

    /// Text and no audio is the speech model's refusal, carried the same way the image
    /// path carries it — and the message names audio, not an image.
    #[test]
    fn text_with_no_audio_names_audio_and_carries_what_was_said() {
        let body = pcm_response(serde_json::json!([{"text": "I can't read that aloud."}]));
        let msg = parse_response(200, &body, MediaOutput::Audio)
            .expect_err("no audio")
            .to_string();
        assert!(
            msg.contains("no audio") && msg.contains("I can't read that aloud."),
            "{msg}"
        );
        assert!(!msg.contains("image"), "{msg}");
    }

    /// Speech takes no input images, so the model says so and the arm refuses them
    /// rather than sending a prompt with the picture dropped.
    #[test]
    fn a_speech_model_accepts_no_inputs_and_an_image_model_does() {
        use crate::media::MediaModel as _;
        let client =
            GeminiMediaClient::new("k", "http://127.0.0.1:9", Duration::from_secs(1)).unwrap();
        assert!(GeminiMediaModel::from_parts(&client, "m", MediaOutput::Image).accepts_inputs());
        assert!(!GeminiMediaModel::from_parts(&client, "m", MediaOutput::Audio).accepts_inputs());
    }

    #[test]
    fn an_empty_candidate_list_is_refused_clearly() {
        let err = parse_response(200, br#"{"candidates":[]}"#, MediaOutput::Image)
            .expect_err("no candidates");
        assert!(matches!(err, GeminiMediaError::NoCandidates));
    }
}
