//! The OpenAI Images API — kaibo's second media kind (`ProviderKind::OpenAiImages`),
//! a sibling of `src/stability.rs`, not a redesign of it.
//!
//! # One kind, two real targets
//!
//! `POST {base}/images/generations` is spoken by two things kaibo cares about:
//! hosted OpenAI image generation (the gpt-image-1 family, at
//! `https://api.openai.com/v1`) and local stable-diffusion.cpp's `sd-server`, which
//! implements the same endpoint shape on a keyless local port. That is why this is
//! ONE kind with a configurable `base_url` (default the hosted endpoint) and an
//! optional key (`key_optional` seeds true — the sd-server case; an operator dialing
//! hosted OpenAI sets `key_optional = false` explicitly), mirroring the `openai`
//! completion kind's posture rather than Stability's.
//!
//! # Sync-only, and b64 or error
//!
//! Every operation here is synchronous: the POST returns the artifacts in-call, so
//! [`crate::media::MediaModel::generate`] always resolves to
//! [`crate::media::MediaOutcome::Complete`] and `poll` is unreachable (it bails
//! loudly — see its doc). The `n` field makes several artifacts per call the NORMAL
//! case, the first real exerciser of the outcome's `Vec` widening: order is
//! preserved, one [`crate::media::MediaArtifact`] per `data` entry.
//!
//! Artifacts must arrive as base64 (`b64_json`). gpt-image-1 always returns b64 and
//! rejects the `response_format` parameter outright; older families (dall-e-2/3)
//! default to URLs unless asked. So [`build_request_body`] seeds
//! `response_format = "b64_json"` for every model family EXCEPT gpt-image-1's, and
//! [`parse_response`] refuses a URL-only entry loudly — kaibo never fetches artifact
//! URLs (a second network hop to an address the provider chose is a different trust
//! shape than decoding bytes already in hand).
//!
//! # Mime: from the request, not the response
//!
//! Unlike Stability, the response carries no artifact content-type — `b64_json` is
//! bytes in a JSON envelope. The format is whatever the request's `output_format`
//! field asked for (`png` | `jpeg` | `webp`, gpt-image-1's set), default `png`
//! (gpt-image-1's own default). [`OutputFormat`] is that closed set; it maps into
//! the CAS's closed [`crate::cas::Extension`], and an unknown value is a loud error
//! at request build — before any credits are spent — rather than a wrong extension
//! on disk. The response's `revised_prompt` field (dall-e-3 rewrites prompts) is
//! deliberately ignored for now: the provenance sidecar records the operator's
//! prompt, the request that *ran*.
//!
//! # Fields ride verbatim, scalars typed
//!
//! The passthrough posture is unchanged from Stability: `size`, `quality`, `n`,
//! `style`, `output_format`, `background`, ... ride [`crate::media::MediaRequest::fields`]
//! and the provider validates its own knobs; `prompt` and `model` stay reserved at
//! the tool layer. The one wire difference: this API takes a JSON body, not a
//! multipart form, and OpenAI type-checks it (`n` must be a JSON integer, not
//! `"2"`). So [`field_value`] sends a field whose string parses as a bare JSON
//! number or bool as that scalar, and everything else as a string — `n = "2"`
//! becomes `2`, `size = "1024x1024"` stays a string. That is a wire-shape
//! translation, not an allowlist: every field still goes through, kaibo still
//! validates none of them.
//!
//! # Credentials
//!
//! Shared with the `openai` completion kind (`OPENAI_API_KEY` / `~/.openai-key`,
//! placeholder when `key_optional`) — resolution lives on `Backend::resolve_key`;
//! this module takes an already-resolved key. The base URL default is
//! [`crate::credentials::HOSTED_OPENAI_BASE_URL`] — a root *through* `/v1`, to which
//! this client appends its own `/images/generations`, the same
//! client-appends-its-path contract every configurable base URL in kaibo follows.

use std::time::Duration;

use anyhow::Result as AnyResult;
use base64::Engine as _;
use reqwest::header::AUTHORIZATION;
use serde_json::{Map, Value};

use crate::media::{MediaArtifact, MediaJobId, MediaOutcome, MediaPollOutcome, MediaRequest};

// --- Output format -----------------------------------------------------------

/// The closed set of image formats the Images API can be asked for
/// (`output_format`, gpt-image-1's parameter) — and, just as important, the closed
/// set of extensions the CAS can name the bytes under. Parsed case-insensitively;
/// anything else is a loud [`OpenAiImagesError::UnknownOutputFormat`] at request
/// build, before the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
}

impl OutputFormat {
    /// Parse a caller's `output_format` field value. The accepted spellings are
    /// exactly the API's own (`png` | `jpeg` | `webp`), case-insensitive; `"jpg"` is
    /// not among them — OpenAI rejects it too, so accepting it here would build a
    /// request the provider refuses.
    pub fn parse(value: &str) -> Result<Self, OpenAiImagesError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "png" => Ok(OutputFormat::Png),
            "jpeg" => Ok(OutputFormat::Jpeg),
            "webp" => Ok(OutputFormat::Webp),
            other => Err(OpenAiImagesError::UnknownOutputFormat(other.to_string())),
        }
    }

    /// The wire mime for artifacts of this format — what
    /// [`crate::media::MediaArtifact::mime`] carries.
    pub fn mime(self) -> &'static str {
        match self {
            OutputFormat::Png => "image/png",
            OutputFormat::Jpeg => "image/jpeg",
            OutputFormat::Webp => "image/webp",
        }
    }

    /// The CAS extension the bytes are stored under. Total, unlike Stability's
    /// `MediaType::to_cas_extension`: this enum was drawn to fit inside
    /// [`crate::cas::Extension`] from the start.
    pub fn to_cas_extension(self) -> crate::cas::Extension {
        match self {
            OutputFormat::Png => crate::cas::Extension::Png,
            OutputFormat::Jpeg => crate::cas::Extension::Jpeg,
            OutputFormat::Webp => crate::cas::Extension::Webp,
        }
    }
}

// --- Errors ------------------------------------------------------------------

/// Everything that can go wrong building or interpreting an Images API call. Own
/// type for the same reason `StabilityError` is: [`build_request_body`] and
/// [`parse_response`] stay pure and unit-testable with no reqwest types in the
/// loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiImagesError {
    /// The HTTP request itself failed (DNS, connect, TLS, a dropped connection) —
    /// message pre-rendered so this stays `Clone`/`PartialEq`.
    Transport(String),
    /// A non-2xx response. `body` is OpenAI's `{error: {message, ...}}` rendered
    /// readably when it parses; the raw text otherwise (a local sd-server or a
    /// gateway may not speak OpenAI's error shape, and the real bytes beat
    /// swallowing them).
    Provider { status: u16, body: String },
    /// A 2xx whose body didn't parse as the documented `{data: [...]}` envelope.
    InvalidBody(String),
    /// A 2xx with an empty `data` array — a provider bug this module refuses at its
    /// own impl rather than handing an empty artifact list downstream (see
    /// `MediaOutcome::Complete`'s contract).
    EmptyData,
    /// A `data` entry with no `b64_json`. `has_url` distinguishes the real-world
    /// case (an older model family defaulted to URLs) from a malformed entry, so
    /// the message can name the fix.
    MissingB64 { index: usize, has_url: bool },
    /// A `b64_json` value that didn't decode as base64.
    InvalidB64 { index: usize, detail: String },
    /// An `output_format` field naming a format outside the closed
    /// [`OutputFormat`] set — refused at request build, before the call.
    UnknownOutputFormat(String),
}

impl std::fmt::Display for OpenAiImagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenAiImagesError::Transport(msg) => {
                write!(f, "Images API request failed: {msg}")
            }
            OpenAiImagesError::Provider { status, body } => {
                write!(f, "Images API returned {status}: {body}")
            }
            OpenAiImagesError::InvalidBody(msg) => write!(
                f,
                "Images API 2xx body didn't parse as {{\"data\": [...]}}: {msg}"
            ),
            OpenAiImagesError::EmptyData => write!(
                f,
                "Images API returned an empty `data` array — zero artifacts is never \
                 treated as a successful generation"
            ),
            OpenAiImagesError::MissingB64 { index, has_url } => {
                if *has_url {
                    write!(
                        f,
                        "Images API data[{index}] carries a URL instead of `b64_json` — \
                         kaibo never fetches artifact URLs; artifacts must arrive as \
                         base64. This model family defaults to URLs and appears to have \
                         ignored (or been overridden on) `response_format = \"b64_json\"`"
                    )
                } else {
                    write!(
                        f,
                        "Images API data[{index}] has no `b64_json` — artifacts must \
                         arrive as base64, and this entry carries none"
                    )
                }
            }
            OpenAiImagesError::InvalidB64 { index, detail } => write!(
                f,
                "Images API data[{index}].b64_json is not valid base64: {detail}"
            ),
            OpenAiImagesError::UnknownOutputFormat(v) => write!(
                f,
                "output_format {v:?} is not one this kind can name on disk — expected \
                 png, jpeg, or webp (the Images API's own set)"
            ),
        }
    }
}

impl std::error::Error for OpenAiImagesError {}

// --- Request building (pure) -------------------------------------------------

/// A field value as the JSON scalar the API type-checks for: a string that parses
/// as a bare JSON number or bool goes typed (`"2"` → `2`, `"true"` → `true`);
/// everything else stays a string (`"1024x1024"`, `"high"`). See the module doc —
/// this is wire-shape translation, not validation.
fn field_value(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ (Value::Number(_) | Value::Bool(_))) => v,
        _ => Value::String(raw.to_string()),
    }
}

/// True for the model family that always returns base64 and REJECTS the
/// `response_format` parameter (gpt-image-1, gpt-image-1-mini, and successors under
/// the same prefix). Everything else — dall-e-2/3, and local sd-server models under
/// arbitrary ids — takes the parameter, so it gets seeded.
fn always_b64(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("gpt-image")
}

/// Build the JSON body for one generation call, and resolve the format its
/// artifacts will be in. Pure.
///
/// - `model` and `prompt` come from the slot and the tool parameter (the tool layer
///   reserves both field names, so they cannot arrive in `request.fields`).
/// - `response_format = "b64_json"` is seeded for every model family except
///   gpt-image-1's (which rejects the parameter and always returns b64) — a caller
///   field of the same name replaces the seed, the same last-write-wins merge
///   Stability's `output_format` seed follows. Overriding it to `"url"` just moves
///   the failure to [`parse_response`]'s loud b64 refusal.
/// - Every `request.fields` entry rides through [`field_value`], in order.
/// - The artifact format is the request's `output_format` field when present
///   ([`OutputFormat::parse`] — unknown is a loud error here, before the call),
///   else png.
pub fn build_request_body(
    model: &str,
    request: &MediaRequest,
) -> Result<(Value, OutputFormat), OpenAiImagesError> {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model.to_string()));
    body.insert("prompt".to_string(), Value::String(request.prompt.clone()));
    if !always_b64(model) {
        body.insert(
            "response_format".to_string(),
            Value::String("b64_json".to_string()),
        );
    }

    let mut format = OutputFormat::Png;
    for (name, value) in &request.fields {
        if name == "output_format" {
            format = OutputFormat::parse(value)?;
        }
        // Map insert: a caller field replaces a seeded default of the same name
        // (fields are uniquely named by the tool layer's map face).
        body.insert(name.clone(), field_value(value));
    }
    Ok((Value::Object(body), format))
}

// --- Response handling (pure) ------------------------------------------------

/// OpenAI's error body, when it is OpenAI's — rendered readably, with the raw text
/// as the fallback for anything else in front of (or instead of) the real API.
fn provider_error(status: u16, body: &[u8]) -> OpenAiImagesError {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: ErrorDetail,
    }
    #[derive(serde::Deserialize)]
    struct ErrorDetail {
        message: String,
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    let rendered = match serde_json::from_slice::<ErrorBody>(body) {
        Ok(e) => match e.error.kind {
            Some(kind) => format!("{} ({kind})", e.error.message),
            None => e.error.message,
        },
        Err(_) => String::from_utf8_lossy(body).trim().to_string(),
    };
    OpenAiImagesError::Provider {
        status,
        body: rendered,
    }
}

/// Interpret one HTTP response as the artifact list. A non-2xx is always a provider
/// error. A 2xx must carry `{data: [...]}` with at least one entry, every entry
/// carrying decodable `b64_json` — order preserved, one artifact per entry, each
/// stamped with `format`'s mime. The API reports no seed, so every artifact's
/// `seed` is `None`. (`data[].revised_prompt` may be present; ignored — see the
/// module doc.)
pub fn parse_response(
    status: u16,
    body: &[u8],
    format: OutputFormat,
) -> Result<Vec<MediaArtifact>, OpenAiImagesError> {
    if !(200..300).contains(&status) {
        return Err(provider_error(status, body));
    }
    let v: Value =
        serde_json::from_slice(body).map_err(|e| OpenAiImagesError::InvalidBody(e.to_string()))?;
    let Some(data) = v.get("data").and_then(Value::as_array) else {
        return Err(OpenAiImagesError::InvalidBody(
            "no `data` array in the response".to_string(),
        ));
    };
    if data.is_empty() {
        return Err(OpenAiImagesError::EmptyData);
    }
    let mut artifacts = Vec::with_capacity(data.len());
    for (index, entry) in data.iter().enumerate() {
        let Some(b64) = entry.get("b64_json").and_then(Value::as_str) else {
            return Err(OpenAiImagesError::MissingB64 {
                index,
                has_url: entry.get("url").and_then(Value::as_str).is_some(),
            });
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| OpenAiImagesError::InvalidB64 {
                index,
                detail: e.to_string(),
            })?;
        artifacts.push(MediaArtifact {
            bytes,
            mime: format.mime().to_string(),
            // The Images API reports no seed — there is nothing to record for
            // reproduction, so the provenance sidecar's seed stays empty.
            seed: None,
        });
    }
    Ok(artifacts)
}

// --- The HTTP client ---------------------------------------------------------

/// An Images API connection: the `reqwest::Client` (ring-installed —
/// [`crate::tls::https_client`] is the one build site in this codebase), the
/// resolved key, and the base URL (a root through `/v1`; this client appends
/// `/images/generations`).
#[derive(Clone)]
pub struct OpenAiImagesClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenAiImagesClient {
    /// Build a client from an already-resolved key (see `Backend::resolve_key` —
    /// the placeholder for the keyless local case arrives here like any other key;
    /// a local server ignores the bearer value).
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        request_timeout: Duration,
    ) -> AnyResult<Self> {
        Ok(Self {
            http: crate::tls::https_client(request_timeout)?,
            api_key: api_key.into(),
            base_url: base_url.into(),
        })
    }

    /// Run one generation call: build the JSON body ([`build_request_body`]), POST
    /// it, and hand the response through [`parse_response`]. The one
    /// side-effecting wrapper over this module's pure functions.
    pub async fn generate(
        &self,
        model: &str,
        request: &MediaRequest,
    ) -> Result<Vec<MediaArtifact>, OpenAiImagesError> {
        let (body, format) = build_request_body(model, request)?;
        let url = format!("{}/images/generations", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| OpenAiImagesError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OpenAiImagesError::Transport(e.to_string()))?;
        parse_response(status, &bytes, format)
    }
}

// --- The MediaModel impl -----------------------------------------------------

/// The [`crate::media::MediaModel`] behind an `image = "backend/model-id"` slot on
/// an `openai-images` backend — built by `MediaArm::from_slot` (`src/media.rs`).
#[derive(Clone)]
pub struct OpenAiImagesModel {
    client: OpenAiImagesClient,
    model: String,
}

impl OpenAiImagesModel {
    /// Build from a client + model id — the constructor `MediaArm::from_slot` uses,
    /// mirroring `StabilityImageModel::from_parts`.
    pub fn from_parts(client: &OpenAiImagesClient, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::media::MediaModel for OpenAiImagesModel {
    /// Always resolves to [`MediaOutcome::Complete`] — the Images API is
    /// synchronous, and `n` makes a multi-artifact list the normal shape.
    async fn generate(&self, request: &MediaRequest) -> AnyResult<MediaOutcome> {
        let artifacts = self.client.generate(&self.model, request).await?;
        Ok(MediaOutcome::Complete(artifacts))
    }

    /// Unreachable in practice: `generate` above never returns
    /// [`MediaOutcome::Deferred`], so no caller ever holds a job id to poll this
    /// kind with. It bails loudly rather than pretending — a poll arriving here
    /// means some caller invented a job id or a future change broke the sync-only
    /// declaration, and either deserves a crash over a silent `Pending` forever.
    async fn poll(&self, _job: &MediaJobId) -> AnyResult<MediaPollOutcome> {
        anyhow::bail!(
            "openai-images declares no deferred operations — every generation \
             completes in-call, so there is no provider job to poll"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn request(fields: &[(&str, &str)]) -> MediaRequest {
        MediaRequest {
            prompt: "a lighthouse at dusk".to_string(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            input_image: None,
        }
    }

    // --- OutputFormat -----------------------------------------------------

    /// The closed set, its mimes, and its CAS extensions — the mapping the stored
    /// artifact's on-disk name rides on.
    #[test]
    fn output_format_maps_to_mime_and_cas_extension() {
        for (raw, format, mime, ext) in [
            (
                "png",
                OutputFormat::Png,
                "image/png",
                crate::cas::Extension::Png,
            ),
            (
                "jpeg",
                OutputFormat::Jpeg,
                "image/jpeg",
                crate::cas::Extension::Jpeg,
            ),
            (
                "webp",
                OutputFormat::Webp,
                "image/webp",
                crate::cas::Extension::Webp,
            ),
            // Case-insensitive, like every media-type comparison in this codebase.
            (
                "PNG",
                OutputFormat::Png,
                "image/png",
                crate::cas::Extension::Png,
            ),
        ] {
            assert_eq!(OutputFormat::parse(raw), Ok(format), "input {raw:?}");
            assert_eq!(format.mime(), mime);
            assert_eq!(format.to_cas_extension(), ext);
        }
    }

    /// An unknown output_format is a loud error naming the value and the accepted
    /// set — including `jpg`, which OpenAI itself rejects, so accepting it here
    /// would only move the failure to the provider.
    #[test]
    fn output_format_unknown_fails_loudly() {
        for raw in ["gif", "avif", "jpg", ""] {
            let err = OutputFormat::parse(raw).unwrap_err();
            assert!(
                matches!(err, OpenAiImagesError::UnknownOutputFormat(_)),
                "input {raw:?} got {err:?}"
            );
        }
        let msg = OutputFormat::parse("gif").unwrap_err().to_string();
        assert!(
            msg.contains("gif") && msg.contains("png"),
            "the message names what arrived and the accepted set, got: {msg}"
        );
    }

    // --- build_request_body -----------------------------------------------

    /// The dall-e-and-local families get `response_format = "b64_json"` seeded —
    /// they default to URLs, which kaibo never fetches.
    #[test]
    fn request_body_seeds_b64_response_format_for_url_defaulting_families() {
        for model in ["dall-e-3", "dall-e-2", "sd3.5-large", "whatever-local"] {
            let (body, format) = build_request_body(model, &request(&[])).unwrap();
            assert_eq!(
                body.get("response_format").and_then(Value::as_str),
                Some("b64_json"),
                "model {model:?}"
            );
            assert_eq!(body.get("model").and_then(Value::as_str), Some(model));
            assert_eq!(
                body.get("prompt").and_then(Value::as_str),
                Some("a lighthouse at dusk")
            );
            assert_eq!(format, OutputFormat::Png, "png is the default format");
        }
    }

    /// The gpt-image family always returns b64 and REJECTS the parameter — seeding
    /// it there would fail every hosted call.
    #[test]
    fn request_body_omits_response_format_for_gpt_image_family() {
        for model in ["gpt-image-1", "gpt-image-1-mini", "GPT-IMAGE-2"] {
            let (body, _) = build_request_body(model, &request(&[])).unwrap();
            assert!(
                body.get("response_format").is_none(),
                "model {model:?} must not carry response_format, got {body}"
            );
        }
    }

    /// Fields ride through verbatim in value, with bare JSON scalars typed the way
    /// the API type-checks them: `n = "2"` must go as the integer 2 (OpenAI rejects
    /// the string), `size` stays a string. A caller's `response_format` replaces
    /// the seeded default rather than duplicating it.
    #[test]
    fn request_body_types_scalars_and_lets_fields_override_seeds() {
        let (body, format) = build_request_body(
            "dall-e-3",
            &request(&[
                ("n", "2"),
                ("size", "1024x1024"),
                ("quality", "high"),
                ("output_format", "webp"),
                ("response_format", "b64_json"),
            ]),
        )
        .unwrap();
        assert_eq!(body.get("n"), Some(&Value::from(2)), "n goes typed");
        assert_eq!(
            body.get("size").and_then(Value::as_str),
            Some("1024x1024"),
            "size stays a string"
        );
        assert_eq!(body.get("quality").and_then(Value::as_str), Some("high"));
        assert_eq!(format, OutputFormat::Webp, "output_format drives the mime");
        assert_eq!(
            body.get("response_format").and_then(Value::as_str),
            Some("b64_json"),
            "one response_format, not two"
        );
    }

    /// An unknown output_format fails at request build — before any credits are
    /// spent — not after the provider generated under a format the CAS can't name.
    #[test]
    fn request_body_refuses_unknown_output_format_before_the_call() {
        let err =
            build_request_body("gpt-image-1", &request(&[("output_format", "avif")])).unwrap_err();
        assert_eq!(
            err,
            OpenAiImagesError::UnknownOutputFormat("avif".to_string())
        );
    }

    // --- parse_response ----------------------------------------------------

    /// One image: one artifact, bytes decoded, mime from the request's format,
    /// seed None (the API reports none).
    #[test]
    fn parse_response_single_image() {
        let body = serde_json::json!({
            "created": 1_722_000_000u64,
            "data": [{ "b64_json": b64(b"png-bytes") }],
        });
        let artifacts =
            parse_response(200, body.to_string().as_bytes(), OutputFormat::Png).unwrap();
        assert_eq!(
            artifacts,
            vec![MediaArtifact {
                bytes: b"png-bytes".to_vec(),
                mime: "image/png".to_string(),
                seed: None,
            }]
        );
    }

    /// `n > 1`: every artifact lands, in the provider's order — the first real
    /// exerciser of the outcome's Vec widening.
    #[test]
    fn parse_response_multi_image_preserves_order() {
        let body = serde_json::json!({
            "data": [
                { "b64_json": b64(b"first") },
                { "b64_json": b64(b"second"), "revised_prompt": "a nicer lighthouse" },
                { "b64_json": b64(b"third") },
            ],
        });
        let artifacts =
            parse_response(200, body.to_string().as_bytes(), OutputFormat::Webp).unwrap();
        assert_eq!(
            artifacts
                .iter()
                .map(|a| a.bytes.clone())
                .collect::<Vec<_>>(),
            vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()],
            "one artifact per data entry, order preserved"
        );
        assert!(
            artifacts.iter().all(|a| a.mime == "image/webp"),
            "every artifact carries the request's format"
        );
    }

    /// A URL-only entry is refused loudly, naming the b64 requirement — kaibo never
    /// fetches artifact URLs.
    #[test]
    fn parse_response_url_only_entry_is_refused_naming_b64() {
        let body = serde_json::json!({
            "data": [
                { "b64_json": b64(b"fine") },
                { "url": "https://oaidalleapi.example/img.png" },
            ],
        });
        let err = parse_response(200, body.to_string().as_bytes(), OutputFormat::Png).unwrap_err();
        assert_eq!(
            err,
            OpenAiImagesError::MissingB64 {
                index: 1,
                has_url: true
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("b64_json") && msg.contains("never fetches"),
            "the message names the b64 requirement and the no-URL-fetch stance, got: {msg}"
        );
    }

    /// An entry with neither b64 nor URL is the malformed sibling — same refusal,
    /// different rendering.
    #[test]
    fn parse_response_entry_with_no_b64_and_no_url_is_refused() {
        let body = serde_json::json!({ "data": [{ "revised_prompt": "only this" }] });
        let err = parse_response(200, body.to_string().as_bytes(), OutputFormat::Png).unwrap_err();
        assert_eq!(
            err,
            OpenAiImagesError::MissingB64 {
                index: 0,
                has_url: false
            }
        );
    }

    /// Invalid base64 is a loud error naming the entry, never truncated-or-empty
    /// bytes handed on as an artifact.
    #[test]
    fn parse_response_invalid_base64_is_refused() {
        let body = serde_json::json!({ "data": [{ "b64_json": "not!!valid@@base64" }] });
        let err = parse_response(200, body.to_string().as_bytes(), OutputFormat::Png).unwrap_err();
        assert!(
            matches!(err, OpenAiImagesError::InvalidB64 { index: 0, .. }),
            "got {err:?}"
        );
    }

    /// Zero artifacts is never a success.
    #[test]
    fn parse_response_empty_data_is_refused() {
        let body = serde_json::json!({ "data": [] });
        let err = parse_response(200, body.to_string().as_bytes(), OutputFormat::Png).unwrap_err();
        assert_eq!(err, OpenAiImagesError::EmptyData);
    }

    /// A 2xx that isn't the documented envelope at all — a proxy page, a bare
    /// string — is refused as an invalid body, not sniffed.
    #[test]
    fn parse_response_non_envelope_body_is_refused() {
        for body in [&b"<html>gateway</html>"[..], b"{}", b"{\"data\": \"nope\"}"] {
            let err = parse_response(200, body, OutputFormat::Png).unwrap_err();
            assert!(
                matches!(err, OpenAiImagesError::InvalidBody(_)),
                "body {:?} got {err:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// A non-2xx renders OpenAI's own error shape readably, and falls back to the
    /// raw text for anything else in front of the API.
    #[test]
    fn parse_response_provider_error_renders_openai_shape_and_raw_fallback() {
        let openai = serde_json::json!({
            "error": { "message": "Billing hard limit reached", "type": "invalid_request_error" }
        });
        let err =
            parse_response(400, openai.to_string().as_bytes(), OutputFormat::Png).unwrap_err();
        assert_eq!(
            err,
            OpenAiImagesError::Provider {
                status: 400,
                body: "Billing hard limit reached (invalid_request_error)".to_string()
            }
        );

        let err = parse_response(502, b"Bad Gateway", OutputFormat::Png).unwrap_err();
        assert_eq!(
            err,
            OpenAiImagesError::Provider {
                status: 502,
                body: "Bad Gateway".to_string()
            }
        );
    }

    // --- the MediaModel seam ----------------------------------------------

    /// `poll` bails loudly: this kind declares no deferred operations, so a poll
    /// arriving at all is a caller inventing a job id or a broken sync-only
    /// declaration — crash over a silent forever-Pending.
    #[tokio::test]
    async fn poll_bails_loudly() {
        let client =
            OpenAiImagesClient::new("test-key", "http://localhost:1", Duration::from_secs(1))
                .expect("client construction is pure config, no network");
        let model = OpenAiImagesModel::from_parts(&client, "gpt-image-1");
        let err = crate::media::MediaModel::poll(&model, &MediaJobId("job-x".to_string()))
            .await
            .expect_err("poll must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no deferred operations"),
            "the refusal names the declaration, got: {msg}"
        );
    }
}
