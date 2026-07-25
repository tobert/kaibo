//! Stability AI's image-generation family — kaibo's own facade, with rig's
//! `image_generation` trait wired as a thin adapter over it.
//!
//! # Why a facade first, not a direct trait impl
//!
//! rig ships `openai`/`xai`/`azure`/`huggingface` implementations of
//! [`rig_core::image_generation::ImageGenerationModel`], and that trait's request shape
//! (`prompt`/`width`/`height`/`additional_params`) is deliberately thin — it fits a
//! single text-to-image call and nothing else. Stability's v2beta API is bigger than
//! that: alongside `stable-image/generate/{core,ultra,sd3}` (the only family wired
//! here), it also ships upscale (conservative/creative/fast), edit
//! (inpaint/outpaint/erase/search-and-replace/remove-background), control
//! (sketch/structure/style), and image-to-video — and every one of those, unlike
//! generate, takes an **input image** alongside its other fields. Building straight
//! to rig's shape would mean rewriting this module's core type the day any of those
//! lands. So the primary abstraction here is [`StabilityRequest`]/[`Operation`] —
//! provider-native, already carrying an optional input image — and
//! [`StabilityImageModel`]'s [`rig_core::image_generation::ImageGenerationModel`] impl
//! is a thin translation at the bottom ([`from_rig_request`]) that only text-to-image
//! ever needs. Adding the next operation means one new `Operation` variant, one
//! `path()` arm, and (if it needs a rig-facing surface at all — most won't, since rig's
//! trait has no concept of upscale/edit) a second adapter function; [`StabilityRequest`],
//! [`build_form_fields`], and [`handle_response`] need no change, because every v2beta
//! synchronous image endpoint shares one shape: multipart in, image bytes (or a
//! moderation refusal) out.
//!
//! # The wire, confirmed live (2026-07-25)
//!
//! Verified against `https://api.stability.ai/v2alpha/openapi` (the spec backing
//! `platform.stability.ai/docs/api-reference`, which is a client-rendered app with no
//! static HTML to scrape — the OpenAPI JSON is the actual source of truth) *and* one
//! real `generate/core` call:
//!
//! - `POST https://api.stability.ai/v2beta/stable-image/generate/{core,ultra,sd3}`,
//!   body `multipart/form-data`, `authorization: Bearer <key>`.
//! - `accept: image/*` (what this module always sends) gets a `200` with `content-type:
//!   image/{png,jpeg,webp}` and the **raw bytes** as the body — confirmed with a live
//!   `core` call: a real 1536×1536 PNG, 3,117,216 bytes. `accept: application/json`
//!   gets base64 JSON instead; this module never asks for that path (see
//!   [`handle_response`]'s doc for why a `200` with a JSON content-type is treated as an
//!   error rather than a second success path to parse).
//! - **`aspect_ratio`, not `width`/`height`** — confirmed both by the spec and the live
//!   call (`aspect_ratio=1:1` is what actually worked). rig's `ImageGenerationRequest`
//!   only carries width/height, so [`aspect_ratio_for`] bridges the two.
//! - Every non-2xx response (`400`/`403`/`422`/`429`/`500`, and the dedicated
//!   `403 content_moderation` shape) shares one JSON body: `{id, name, errors: [...]}}`
//!   — confirmed live: an empty POST (no body at all) came back `400` with
//!   `{"errors":["content-type: required"],"id":"...","name":"bad_request"}`.
//! - **Two response headers matter as much as the body, and both are handled here —
//!   this was missed in the original design pass and corrected before ship:**
//!   - `finish-reason: SUCCESS` — a `200` with an `image/*` content-type is *not*
//!     sufficient to call a generation successful. Stability returns `200` with
//!     `finish-reason: CONTENT_FILTERED` when its moderation trips, and the body is a
//!     blurred/blank image at that point — real bytes, wrong content. Treating that as
//!     success would store a filtered placeholder under a valid content digest forever
//!     once the CAS wiring lands, indistinguishable from a real result. So
//!     [`handle_response`] treats **any** `finish-reason` other than the literal
//!     `"SUCCESS"` as a loud, typed error ([`StabilityError::NotSuccess`]) — including
//!     values this module has never seen, since Stability's enum could grow one without
//!     us finding out first. A `200` with no `finish-reason` header at all is *also*
//!     refused ([`StabilityError::MissingFinishReason`]): the spec documents it as
//!     always present on a binary response, so its absence means something between us
//!     and Stability isn't behaving as documented, not that everything's fine.
//!   - `seed` — the value Stability actually used (it fills one in when the caller
//!     didn't pin one). Captured into [`GeneratedImage::seed`] rather than discarded:
//!     the whole stewardship argument for the media CAS (`docs/media-cas.md`) is that a
//!     generated image may never be reproducible again, so throwing away the one field
//!     that aids reproduction would undercut that argument on day one. The next stage
//!     wires this into the CAS's `Provenance` sidecar.
//!
//! # width/height → aspect_ratio
//!
//! [`aspect_ratio_for`] picks the nearest of Stability's nine supported ratios by
//! **log**-distance rather than linear difference — see that function's doc for why the
//! choice is deliberate (a linear metric is not symmetric under the width/height swap
//! that turns e.g. `16:9` into `9:16`).
//!
//! # Credentials
//!
//! Through [`crate::credentials::resolve`] — the same env-wins-over-dotfile core every
//! keyed `ProviderKind` in `credentials.rs` already uses — not a second mechanism.
//! Stability isn't a `ProviderKind` yet (that enum/config/cast wiring is explicitly the
//! *next* stage; see `docs/media-cas.md`'s "Open" list), so [`STABILITY_KEY_ENV_VAR`]/
//! [`STABILITY_KEY_FILE_NAME`] and [`resolve_key`] live here rather than there until it
//! is. Nothing in this module hardcodes a path to any specific machine's key-file — the
//! file name is a fixed convention (matching every sibling provider), the directory
//! (`$HOME`) is resolved at call time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result as AnyResult};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use rig_core::image_generation::{
    ImageGenerationError, ImageGenerationModel as RigImageGenerationModel, ImageGenerationRequest,
    ImageGenerationResponse,
};
use serde::Deserialize;
use serde_json::Value;

use crate::credentials;

/// Stability's API host. Overridable per [`StabilityClient::new`] call (tests point it
/// at nothing; a future config layer could point it at a proxy), matching how every
/// other keyed backend in this codebase treats its base URL as data, not a literal
/// baked into the request path.
pub const STABILITY_API_BASE: &str = "https://api.stability.ai";

/// The env var that overrides the key-file — mirrors every `ProviderKind` in
/// `credentials.rs` (env wins over dotfile).
pub const STABILITY_KEY_ENV_VAR: &str = "STABILITY_API_KEY";

/// The key-file's name within `$HOME`, matching the naming convention every
/// `ProviderKind` key file already follows (`.anthropic-key.txt`, `.deepseek-key`, ...).
pub const STABILITY_KEY_FILE_NAME: &str = ".stability-key.txt";

/// Resolve the Stability key from an explicit env value and `$HOME`: the env value
/// wins over `~/`[`STABILITY_KEY_FILE_NAME`], mirroring `credentials::resolve`'s own
/// precedence. Pure — both inputs are parameters, not read from the real process
/// environment — so it's testable with no risk of the exact race a real-env-mutating
/// test would hit under cargo's default parallel test execution (confirmed the hard
/// way while writing this module's tests: a test that called `std::env::set_var` on
/// this key raced a sibling test reading it and failed intermittently). [`resolve_key`]
/// is the real-`$HOME`/real-env convenience most callers want.
pub fn resolve_key_from(env_value: Option<&str>, home: &Path) -> AnyResult<String> {
    credentials::resolve(env_value, &home.join(STABILITY_KEY_FILE_NAME))
        .context("loading Stability credentials")
}

/// [`resolve_key_from`] against the real environment and `$HOME`.
pub fn resolve_key() -> AnyResult<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME is not set; cannot locate the Stability key-file"))?;
    let env_value = std::env::var(STABILITY_KEY_ENV_VAR).ok();
    resolve_key_from(env_value.as_deref(), &home)
}

// --- The facade's own request/response shape --------------------------------

/// One request to a Stability v2beta image operation — provider-native (Stability's
/// own `aspect_ratio`, not rig's `width`/`height`) and already shaped for growth. See
/// the module doc for why `input_image` exists even though the only operation wired so
/// far ([`Operation::Generate`]) never sets it.
#[derive(Debug, Clone, Default)]
pub struct StabilityRequest {
    /// Mandatory for every operation wired so far. A future prompt-less operation
    /// (`remove-background`, `erase`) would need this widened to `Option<String>` —
    /// noted rather than solved speculatively; see `docs/media-cas.md`'s "Open" list.
    pub prompt: String,
    /// Bytes of an input image, for operations that take one (every edit/control/
    /// upscale/image-to-video call, and generate's own image-to-image mode). Attached
    /// as a multipart file part named `image`, matching the field name every one of
    /// those operations documents. `None` for text-to-image.
    pub input_image: Option<Vec<u8>>,
    /// Stability's own `aspect_ratio` enum value (e.g. `"16:9"`), when this operation
    /// takes one. The rig adapter ([`from_rig_request`]) derives it from
    /// `ImageGenerationRequest`'s width/height via [`aspect_ratio_for`]; a caller
    /// driving this facade directly can just say what they mean.
    pub aspect_ratio: Option<String>,
    /// Every other form field (`output_format`, `seed`, `style_preset`,
    /// `negative_prompt`, `cfg_scale`, `strength`, `mode`, an explicit `model` for
    /// `sd3`, ...) — provider-specific and growing per operation and per Stability
    /// release, so a typed struct field per knob would need constant upkeep. `(name,
    /// value)` pairs; a later entry with the same name overrides an earlier one (see
    /// [`build_form_fields`]), which is also how an explicit override here can beat a
    /// derived `aspect_ratio`.
    pub fields: Vec<(String, String)>,
}

/// One Stability v2beta endpoint this facade can address. Only [`Operation::Generate`]
/// is wired; `Upscale`/`Edit`/`Control`/`ImageToVideo` are the natural siblings v2beta
/// already documents, each one endpoint path away — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Generate(GenerateRoute),
}

impl Operation {
    /// The path segment after `/v2beta/`.
    fn path(&self) -> String {
        match self {
            Operation::Generate(route) => format!("stable-image/generate/{}", route.path_segment()),
        }
    }
}

/// Which `generate` endpoint a call addresses. Stability's v2beta text-to-image family
/// is three distinct routes, not one endpoint with a `model` field — only `sd3` takes
/// its own `model` form field (the specific SD3.5 variant); `core`/`ultra` are fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerateRoute {
    Core,
    Ultra,
    /// The SD3.5 variant string (`sd3.5-large`, `sd3.5-large-turbo`, `sd3.5-medium`, ...)
    /// rides as the `model` form field — see [`from_rig_request`]. Passed through
    /// verbatim; Stability validates it, so an unrecognized variant still reaches the
    /// provider as a normal `400`, not a client-side guess.
    Sd3 { model: String },
}

impl GenerateRoute {
    /// Classify a rig model id into a route: `"core"`/`"ultra"` (case-insensitive)
    /// address those fixed endpoints; anything else is assumed to name an SD3.5
    /// variant and routes to `sd3`.
    pub fn classify(model_id: &str) -> Self {
        match model_id.to_ascii_lowercase().as_str() {
            "core" => GenerateRoute::Core,
            "ultra" => GenerateRoute::Ultra,
            _ => GenerateRoute::Sd3 {
                model: model_id.to_string(),
            },
        }
    }

    fn path_segment(&self) -> &'static str {
        match self {
            GenerateRoute::Core => "core",
            GenerateRoute::Ultra => "ultra",
            GenerateRoute::Sd3 { .. } => "sd3",
        }
    }
}

/// Every aspect ratio Stability's `stable-image/generate/*` endpoints accept, paired
/// with its numeric ratio (width / height) — shared by `core`, `ultra`, and `sd3`
/// (confirmed against the live `https://api.stability.ai/v2alpha/openapi` spec,
/// 2026-07-25).
const ASPECT_RATIOS: &[(&str, f64)] = &[
    ("21:9", 21.0 / 9.0),
    ("16:9", 16.0 / 9.0),
    ("3:2", 3.0 / 2.0),
    ("5:4", 5.0 / 4.0),
    ("1:1", 1.0),
    ("4:5", 4.0 / 5.0),
    ("2:3", 2.0 / 3.0),
    ("9:16", 9.0 / 16.0),
    ("9:21", 9.0 / 21.0),
];

/// Bridge rig's `width`/`height` onto Stability's `aspect_ratio` enum: the nearest of
/// [`ASPECT_RATIOS`] by **log**-distance, not linear difference. Log-distance is the
/// deliberate choice — it is symmetric under the width/height swap that turns one
/// ratio into its mirror (`16:9` ↔ `9:16`), where a linear difference is not: e.g. a
/// ratio of `1.372` (width 1372, height 1000) sits closer to `5:4` (1.25) than `3:2`
/// (1.5) by linear distance (0.122 vs 0.128), but closer to `3:2` by log-distance
/// (0.0898 vs 0.0926) — and `3:2` is in fact the visually closer ratio (1.372 is much
/// nearer 1.5 proportionally: 1.372/1.5 ≈ 0.915 vs 1.372/1.25 ≈ 1.098). Table-driven in
/// `tests::aspect_ratio_prefers_log_distance_over_linear`.
///
/// Degenerate input (either dimension `0` — unreachable from rig's own builder, whose
/// default is `256x256`, but a caller could still hand-build a zero-sized request) is
/// clamped to `1` before the ratio is taken, so this never divides by zero or produces
/// `NaN`; it resolves to whichever extreme ratio that clamp implies.
pub fn aspect_ratio_for(width: u32, height: u32) -> &'static str {
    let ratio = f64::from(width.max(1)) / f64::from(height.max(1));
    let target = ratio.ln();
    ASPECT_RATIOS
        .iter()
        .min_by(|(_, a), (_, b)| {
            (a.ln() - target)
                .abs()
                .partial_cmp(&(b.ln() - target).abs())
                .expect("ratios and target are always finite, non-NaN floats")
        })
        .map(|(label, _)| *label)
        .expect("ASPECT_RATIOS is non-empty")
}

/// Build the multipart text fields for one call: `prompt`, then `aspect_ratio` (if the
/// request carries one), then every `request.fields` entry — a later entry with a name
/// already present (including `aspect_ratio` itself, letting an explicit override win
/// over a derived one) replaces it rather than duplicating the field. Pure and
/// order-preserving; `request.input_image` is attached separately as a file part by
/// [`StabilityClient::call`], since it isn't a text field.
pub fn build_form_fields(request: &StabilityRequest) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = vec![("prompt".to_string(), request.prompt.clone())];
    if let Some(ar) = &request.aspect_ratio {
        fields.push(("aspect_ratio".to_string(), ar.clone()));
    }
    for (k, v) in &request.fields {
        if let Some(existing) = fields.iter_mut().find(|(name, _)| name == k) {
            existing.1 = v.clone();
        } else {
            fields.push((k.clone(), v.clone()));
        }
    }
    fields
}

// --- Errors -------------------------------------------------------------------

/// Stability's error body shape — shared verbatim across every non-2xx response this
/// module has seen documented (`400`/`403`/`422`/`429`/`500`; confirmed against the
/// live OpenAPI spec, 2026-07-25 — even the dedicated `ContentModerationResponse`
/// schema for `403` is textually identical to the generic error schema, and a live
/// empty POST reproduced it exactly: `{"errors":["content-type: required"],"id":"...",
/// "name":"bad_request"}`).
#[derive(Debug, Deserialize)]
struct StabilityErrorBody {
    id: String,
    name: String,
    errors: Vec<String>,
}

/// Everything that can go wrong building or interpreting a Stability call. Kept as our
/// own type — rather than reaching for rig's `ImageGenerationError` directly — so
/// [`build_form_fields`]/[`from_rig_request`]/[`handle_response`] stay unit-testable
/// with no rig or reqwest types in the loop; [`StabilityImageModel::image_generation`]
/// is the one place this becomes a rig error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StabilityError {
    /// The HTTP request itself failed (DNS, connect, TLS, a dropped connection) —
    /// message pre-rendered so this variant stays `Clone`/`PartialEq` (a `reqwest::Error`
    /// is neither).
    Transport(String),
    /// A non-2xx HTTP response. `body` is the provider's `{id,name,errors}` shape
    /// rendered readably when it parses; the raw text otherwise — a proxy/gateway
    /// error in front of Stability won't be shaped like Stability's own JSON, and
    /// showing the real bytes beats swallowing them.
    Provider { status: u16, body: String },
    /// A `200` whose `content-type` doesn't start with `image/` — the silent-garbage
    /// case this module refuses to paper over. Stability's success path can also return
    /// `application/json` (base64) when the caller asks for it via `Accept:
    /// application/json`; this module always sends `Accept: image/*` (see the module
    /// doc), so any other content-type on a `200` means something upstream isn't
    /// speaking to us the way we asked — never silently decoded as if it might be the
    /// JSON shape.
    UnexpectedContentType { content_type: String },
    /// A `200` with no `content-type` header at all.
    MissingContentType,
    /// A `200`, an `image/*` content-type, and zero bytes — never treated as a
    /// successful empty image.
    EmptyBody,
    /// A `200` with a valid `image/*` body but no `finish-reason` header. The spec
    /// documents this header as always present on a binary response; its absence means
    /// something isn't behaving as documented, so this is refused rather than assumed
    /// fine.
    MissingFinishReason,
    /// A `200` whose `finish-reason` is present but isn't the literal `"SUCCESS"` —
    /// most notably `"CONTENT_FILTERED"` (the generation completed but Stability's
    /// moderation blurred the output), but *any* non-`"SUCCESS"` value is treated the
    /// same way: real bytes, wrong content, never handed back as a successful result.
    NotSuccess { finish_reason: String },
    /// `additional_params` wasn't a JSON object, or one of its values wasn't a
    /// string/number/bool Stability's form fields can carry.
    InvalidAdditionalParams(String),
}

impl std::fmt::Display for StabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StabilityError::Transport(msg) => write!(f, "Stability request failed: {msg}"),
            StabilityError::Provider { status, body } => {
                write!(f, "Stability API returned {status}: {body}")
            }
            StabilityError::UnexpectedContentType { content_type } => write!(
                f,
                "Stability returned 200 with content-type {content_type:?} instead of an \
                 image/* body — refusing to guess at what this is rather than silently \
                 handing back the bytes as if they were an image"
            ),
            StabilityError::MissingContentType => write!(
                f,
                "Stability returned 200 with no content-type header at all — cannot confirm \
                 the body is image bytes, refusing to treat it as one"
            ),
            StabilityError::EmptyBody => write!(
                f,
                "Stability returned 200 with an image/* content-type but an empty body — \
                 never treated as a successful zero-byte image"
            ),
            StabilityError::MissingFinishReason => write!(
                f,
                "Stability returned 200 with image bytes but no finish-reason header — the \
                 API documents this header as always present on a binary response, so its \
                 absence is refused rather than assumed to mean success"
            ),
            StabilityError::NotSuccess { finish_reason } => write!(
                f,
                "Stability finished with finish-reason {finish_reason:?}, not SUCCESS — the \
                 image bytes are real but not a clean result (e.g. CONTENT_FILTERED means \
                 the output was blurred by moderation); refusing to hand it back as a \
                 successful generation"
            ),
            StabilityError::InvalidAdditionalParams(msg) => {
                write!(f, "invalid additional_params: {msg}")
            }
        }
    }
}

impl std::error::Error for StabilityError {}

// --- Response handling ---------------------------------------------------------

/// One successful generation: the image bytes plus whatever Stability told us about
/// them. Kept separate from rig's `ImageGenerationResponse` wrapper so this facade has
/// a return type of its own — a future direct (non-rig) caller (e.g. the media CAS
/// wiring) uses this same shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    /// The seed Stability actually used, from the `seed` response header — present
    /// whenever the provider reports one, whether or not the caller pinned one via
    /// `additional_params`/`fields`. Carried through so a caller can reproduce this
    /// exact generation later (the whole point of capturing it — see the module doc).
    pub seed: Option<String>,
}

/// Interpret one HTTP response's `(status, content-type, finish-reason, seed, body)` —
/// pure, so every failure mode is unit-tested with no network. `Ok` means: a 2xx
/// status, a `content-type` starting with `image/`, a non-empty body, *and* a
/// `finish-reason` header reading exactly `"SUCCESS"` — the only shape this module
/// calls a successful generation. Never returns empty bytes, and never returns a
/// moderation-blurred image, as `Ok`.
pub fn handle_response(
    status: u16,
    content_type: Option<&str>,
    finish_reason: Option<&str>,
    seed: Option<&str>,
    body: &[u8],
) -> Result<GeneratedImage, StabilityError> {
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(body);
        let rendered = match serde_json::from_slice::<StabilityErrorBody>(body) {
            Ok(err) => format!("{} ({}): {}", err.name, err.id, err.errors.join("; ")),
            Err(_) => text.trim().to_string(),
        };
        return Err(StabilityError::Provider {
            status,
            body: rendered,
        });
    }

    let Some(ct) = content_type else {
        return Err(StabilityError::MissingContentType);
    };
    if !ct.starts_with("image/") {
        return Err(StabilityError::UnexpectedContentType {
            content_type: ct.to_string(),
        });
    }
    if body.is_empty() {
        return Err(StabilityError::EmptyBody);
    }

    match finish_reason {
        None => return Err(StabilityError::MissingFinishReason),
        Some(r) if r != "SUCCESS" => {
            return Err(StabilityError::NotSuccess {
                finish_reason: r.to_string(),
            })
        }
        Some(_) => {}
    }

    Ok(GeneratedImage {
        bytes: body.to_vec(),
        seed: seed.map(str::to_string),
    })
}

// --- The rig adapter ------------------------------------------------------------

/// A JSON [`Value`] form field value, as a form-encodable string — Stability's
/// multipart fields are all plain text, so only scalars translate; an
/// array/object/null value means the caller's `additional_params` doesn't shape what
/// this module can send.
fn value_to_form_field(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Translate rig's [`ImageGenerationRequest`] into this facade's own
/// [`StabilityRequest`] + [`Operation`] — the one place rig's thin shape (prompt/width/
/// height/additional_params) meets Stability's native one. `model_id` is
/// [`RigImageGenerationModel::make`]'s model string, classified by
/// [`GenerateRoute::classify`]. `output_format` defaults to `"png"` (Stability's own
/// default, kept explicit rather than omitted so a caller reading this module's
/// behavior never has to check Stability's docs to know what they'll get); an
/// `additional_params` entry of the same name overrides it, same as any other field.
pub fn from_rig_request(
    model_id: &str,
    request: &ImageGenerationRequest,
) -> Result<(Operation, StabilityRequest), StabilityError> {
    let route = GenerateRoute::classify(model_id);
    let mut fields: Vec<(String, String)> = Vec::new();
    if let GenerateRoute::Sd3 { model } = &route {
        fields.push(("model".to_string(), model.clone()));
    }
    fields.push(("output_format".to_string(), "png".to_string()));

    if let Some(extra) = &request.additional_params {
        let obj = extra.as_object().ok_or_else(|| {
            StabilityError::InvalidAdditionalParams(
                "additional_params must be a JSON object of Stability form fields".to_string(),
            )
        })?;
        for (k, v) in obj {
            let value = value_to_form_field(v).ok_or_else(|| {
                StabilityError::InvalidAdditionalParams(format!(
                    "additional_params.{k} must be a string, number, or bool — got {v}"
                ))
            })?;
            if let Some(existing) = fields.iter_mut().find(|(name, _)| name == k) {
                existing.1 = value;
            } else {
                fields.push((k.clone(), value));
            }
        }
    }

    Ok((
        Operation::Generate(route),
        StabilityRequest {
            prompt: request.prompt.clone(),
            input_image: None,
            aspect_ratio: Some(aspect_ratio_for(request.width, request.height).to_string()),
            fields,
        },
    ))
}

// --- The HTTP client -------------------------------------------------------------

/// A Stability API connection: the `reqwest::Client` (ring-installed, per-call
/// deadline — [`crate::tls::https_client`] is the one build site in this codebase) plus
/// the resolved API key and base URL.
#[derive(Clone)]
pub struct StabilityClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl StabilityClient {
    /// Build a client from an already-resolved key (see [`resolve_key`]). `base_url`
    /// defaults to [`STABILITY_API_BASE`] for real use; overridable for tests, or a
    /// future gateway/proxy — the same way every other keyed backend in this codebase
    /// treats its base URL as data.
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

    /// Run one call: build the multipart body from `request` ([`build_form_fields`] for
    /// the text fields, plus an `image` file part when `request.input_image` is set),
    /// POST to `op`'s endpoint with `Accept: image/*`, and hand the response through
    /// [`handle_response`]. The one side-effecting wrapper over this module's two pure
    /// functions.
    pub async fn call(
        &self,
        op: &Operation,
        request: &StabilityRequest,
    ) -> Result<GeneratedImage, StabilityError> {
        let mut form = reqwest::multipart::Form::new();
        for (name, value) in build_form_fields(request) {
            form = form.text(name, value);
        }
        if let Some(bytes) = &request.input_image {
            form = form.part("image", reqwest::multipart::Part::bytes(bytes.clone()));
        }

        let url = format!("{}/v2beta/{}", self.base_url, op.path());
        let resp = self
            .http
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(ACCEPT, "image/*")
            .multipart(form)
            .send()
            .await
            .map_err(|e| StabilityError::Transport(e.to_string()))?;

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let finish_reason = resp
            .headers()
            .get("finish-reason")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let seed = resp
            .headers()
            .get("seed")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let body = resp
            .bytes()
            .await
            .map_err(|e| StabilityError::Transport(e.to_string()))?;

        handle_response(
            status,
            content_type.as_deref(),
            finish_reason.as_deref(),
            seed.as_deref(),
            &body,
        )
    }
}

/// rig's `image_generation` adapter over [`StabilityClient`] — one Stability
/// `generate` model id (`"core"`/`"ultra"`/an SD3.5 variant), classified into a route
/// by [`GenerateRoute::classify`]. Implements
/// [`rig_core::image_generation::ImageGenerationModel`] so a future `image` cast slot
/// can dispatch across this and rig's built-in openai/xai/azure/huggingface impls
/// identically — see the module doc.
#[derive(Clone)]
pub struct StabilityImageModel {
    client: StabilityClient,
    model: String,
}

/// Everything rig's [`RigImageGenerationModel::Response`] carries beyond the image
/// bytes (which ride on rig's own `ImageGenerationResponse::image`): just the seed,
/// today — see [`GeneratedImage::seed`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StabilityImageResponse {
    pub seed: Option<String>,
}

impl RigImageGenerationModel for StabilityImageModel {
    type Response = StabilityImageResponse;
    type Client = StabilityClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }

    async fn image_generation(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse<Self::Response>, ImageGenerationError> {
        let (op, stability_request) = from_rig_request(&self.model, &request)
            .map_err(|e| ImageGenerationError::RequestError(Box::new(e)))?;
        let generated = self
            .client
            .call(&op, &stability_request)
            .await
            .map_err(|e| ImageGenerationError::ProviderError(e.to_string()))?;
        Ok(ImageGenerationResponse {
            image: generated.bytes,
            response: StabilityImageResponse {
                seed: generated.seed,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- aspect_ratio_for ----------------------------------------------------

    #[test]
    fn aspect_ratio_square_is_1_1() {
        assert_eq!(aspect_ratio_for(1024, 1024), "1:1");
    }

    #[test]
    fn aspect_ratio_landscape_and_portrait_are_mirrors() {
        assert_eq!(aspect_ratio_for(1344, 768), "16:9");
        assert_eq!(aspect_ratio_for(768, 1344), "9:16");
        assert_eq!(aspect_ratio_for(1536, 640), "21:9");
        assert_eq!(aspect_ratio_for(640, 1536), "9:21");
    }

    /// The deliberate log-vs-linear divergence: 1372/1000 (1.372) is nearer `5:4`
    /// (1.25) than `3:2` (1.5) by plain linear difference (0.122 vs 0.128), but nearer
    /// `3:2` by log-distance (0.0898 vs 0.0926) — and `3:2` is the proportionally
    /// closer ratio (1.372/1.5 ≈ 0.915 vs 1.372/1.25 ≈ 1.098). A wrong (linear)
    /// implementation would pick `5:4` here.
    #[test]
    fn aspect_ratio_prefers_log_distance_over_linear() {
        assert_eq!(aspect_ratio_for(1372, 1000), "3:2");
        // And its mirror image resolves to the mirrored label — proof the metric is
        // symmetric under the width/height swap, which a linear distance would not
        // guarantee in general.
        assert_eq!(aspect_ratio_for(1000, 1372), "2:3");
    }

    #[test]
    fn aspect_ratio_zero_dimension_does_not_panic() {
        // Unreachable from rig's own builder (default 256x256), but a hand-built
        // request could still zero a dimension — must resolve to an extreme ratio,
        // never divide by zero or produce NaN.
        assert_eq!(aspect_ratio_for(0, 100), "9:21");
        assert_eq!(aspect_ratio_for(100, 0), "21:9");
    }

    // --- build_form_fields -----------------------------------------------------

    #[test]
    fn build_form_fields_prompt_and_aspect_ratio() {
        let req = StabilityRequest {
            prompt: "a cat wearing a hat".to_string(),
            aspect_ratio: Some("16:9".to_string()),
            ..Default::default()
        };
        let fields = build_form_fields(&req);
        assert_eq!(
            fields,
            vec![
                ("prompt".to_string(), "a cat wearing a hat".to_string()),
                ("aspect_ratio".to_string(), "16:9".to_string()),
            ]
        );
    }

    #[test]
    fn build_form_fields_appends_extra_fields_in_order() {
        let req = StabilityRequest {
            prompt: "p".to_string(),
            aspect_ratio: None,
            fields: vec![
                ("output_format".to_string(), "webp".to_string()),
                ("seed".to_string(), "42".to_string()),
            ],
            ..Default::default()
        };
        let fields = build_form_fields(&req);
        assert_eq!(
            fields,
            vec![
                ("prompt".to_string(), "p".to_string()),
                ("output_format".to_string(), "webp".to_string()),
                ("seed".to_string(), "42".to_string()),
            ]
        );
    }

    #[test]
    fn build_form_fields_explicit_field_overrides_derived_aspect_ratio() {
        let req = StabilityRequest {
            prompt: "p".to_string(),
            aspect_ratio: Some("1:1".to_string()),
            fields: vec![("aspect_ratio".to_string(), "16:9".to_string())],
            ..Default::default()
        };
        let fields = build_form_fields(&req);
        assert_eq!(
            fields,
            vec![
                ("prompt".to_string(), "p".to_string()),
                ("aspect_ratio".to_string(), "16:9".to_string()),
            ],
            "an explicit fields entry must win over the derived aspect_ratio, not duplicate it"
        );
    }

    // --- from_rig_request --------------------------------------------------------

    /// Build a real `ImageGenerationRequest` via rig's own builder — the type is
    /// `#[non_exhaustive]`, so a struct literal isn't available outside rig's crate.
    /// Any `ImageGenerationModel` instance can mint the builder; a throwaway
    /// [`StabilityImageModel`] over a never-dialed client does the job with no network.
    fn rig_request(width: u32, height: u32, additional_params: Option<Value>) -> ImageGenerationRequest {
        let client = StabilityClient::new("test-key", "http://localhost", Duration::from_secs(1))
            .expect("client construction is pure config, no network");
        let model = StabilityImageModel::make(&client, "core");
        let mut builder = model
            .image_generation_request()
            .prompt("lighthouse on a cliff")
            .width(width)
            .height(height);
        if let Some(params) = additional_params {
            builder = builder.additional_params(params);
        }
        builder.build()
    }

    #[test]
    fn from_rig_request_core_route_has_no_model_field() {
        let (op, req) = from_rig_request("core", &rig_request(1024, 1024, None)).unwrap();
        assert_eq!(op, Operation::Generate(GenerateRoute::Core));
        assert_eq!(req.aspect_ratio.as_deref(), Some("1:1"));
        assert!(!req.fields.iter().any(|(k, _)| k == "model"));
        assert!(req
            .fields
            .iter()
            .any(|(k, v)| k == "output_format" && v == "png"));
    }

    #[test]
    fn from_rig_request_sd3_variant_carries_model_field() {
        let (op, req) = from_rig_request("sd3.5-large-turbo", &rig_request(1024, 1024, None)).unwrap();
        assert_eq!(
            op,
            Operation::Generate(GenerateRoute::Sd3 {
                model: "sd3.5-large-turbo".to_string()
            })
        );
        assert!(req
            .fields
            .iter()
            .any(|(k, v)| k == "model" && v == "sd3.5-large-turbo"));
    }

    #[test]
    fn from_rig_request_additional_params_override_output_format() {
        let params = serde_json::json!({"output_format": "webp", "seed": 42});
        let (_, req) = from_rig_request("core", &rig_request(1024, 1024, Some(params))).unwrap();
        assert!(req
            .fields
            .iter()
            .any(|(k, v)| k == "output_format" && v == "webp"));
        assert!(req.fields.iter().any(|(k, v)| k == "seed" && v == "42"));
        // output_format must not appear twice (override, not append).
        assert_eq!(
            req.fields.iter().filter(|(k, _)| k == "output_format").count(),
            1
        );
    }

    #[test]
    fn from_rig_request_rejects_non_object_additional_params() {
        let params = serde_json::json!(["not", "an", "object"]);
        let err = from_rig_request("core", &rig_request(1024, 1024, Some(params))).unwrap_err();
        assert!(matches!(err, StabilityError::InvalidAdditionalParams(_)));
    }

    #[test]
    fn from_rig_request_rejects_non_scalar_field_value() {
        let params = serde_json::json!({"style_preset": {"nested": true}});
        let err = from_rig_request("core", &rig_request(1024, 1024, Some(params))).unwrap_err();
        assert!(matches!(err, StabilityError::InvalidAdditionalParams(_)));
    }

    // --- handle_response -----------------------------------------------------

    #[test]
    fn handle_response_success_captures_bytes_and_seed() {
        let body = b"\x89PNG\r\n\x1a\nnotreallyapngbutnonempty";
        let got = handle_response(200, Some("image/png"), Some("SUCCESS"), Some("343940597"), body)
            .expect("expected success");
        assert_eq!(got.bytes, body.to_vec());
        assert_eq!(got.seed.as_deref(), Some("343940597"));
    }

    #[test]
    fn handle_response_non_2xx_parses_provider_error_body() {
        let body = br#"{"id":"abc123","name":"bad_request","errors":["content-type: required"]}"#;
        let err = handle_response(400, Some("application/json"), None, None, body).unwrap_err();
        match err {
            StabilityError::Provider { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("bad_request"));
                assert!(body.contains("abc123"));
                assert!(body.contains("content-type: required"));
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[test]
    fn handle_response_non_2xx_with_unparseable_body_still_surfaces_raw_text() {
        let body = b"<html>502 Bad Gateway</html>";
        let err = handle_response(502, Some("text/html"), None, None, body).unwrap_err();
        match err {
            StabilityError::Provider { status, body } => {
                assert_eq!(status, 502);
                assert!(body.contains("502 Bad Gateway"));
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[test]
    fn handle_response_200_json_content_type_is_the_silent_garbage_case() {
        let body = br#"{"image":"AAAA","seed":1,"finish_reason":"SUCCESS"}"#;
        let err = handle_response(200, Some("application/json"), None, None, body).unwrap_err();
        assert!(matches!(
            err,
            StabilityError::UnexpectedContentType { .. }
        ));
    }

    #[test]
    fn handle_response_200_missing_content_type_is_refused() {
        let err = handle_response(200, None, Some("SUCCESS"), None, b"bytes").unwrap_err();
        assert_eq!(err, StabilityError::MissingContentType);
    }

    #[test]
    fn handle_response_200_empty_body_is_refused() {
        let err = handle_response(200, Some("image/png"), Some("SUCCESS"), None, b"").unwrap_err();
        assert_eq!(err, StabilityError::EmptyBody);
    }

    #[test]
    fn handle_response_200_missing_finish_reason_is_refused() {
        let err = handle_response(200, Some("image/png"), None, None, b"bytes").unwrap_err();
        assert_eq!(err, StabilityError::MissingFinishReason);
    }

    #[test]
    fn handle_response_content_filtered_is_refused_not_silently_returned() {
        let err = handle_response(
            200,
            Some("image/png"),
            Some("CONTENT_FILTERED"),
            Some("1"),
            b"blurredbytes",
        )
        .unwrap_err();
        assert_eq!(
            err,
            StabilityError::NotSuccess {
                finish_reason: "CONTENT_FILTERED".to_string()
            }
        );
    }

    /// Conservative handling of a finish-reason this module has never seen documented:
    /// still refused, not passed through as if it might be fine.
    #[test]
    fn handle_response_unknown_finish_reason_is_refused() {
        let err = handle_response(200, Some("image/png"), Some("SOMETHING_NEW"), None, b"bytes")
            .unwrap_err();
        assert_eq!(
            err,
            StabilityError::NotSuccess {
                finish_reason: "SOMETHING_NEW".to_string()
            }
        );
    }

    // --- credentials -------------------------------------------------------------
    // Pure over an explicit env value + `home` — never touches the real process
    // environment, so these can't race a sibling test the way a real `set_var` would
    // under cargo's default parallel test execution.

    #[test]
    fn resolve_key_from_env_wins_over_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STABILITY_KEY_FILE_NAME), "file-key\n").unwrap();
        let got = resolve_key_from(Some("env-key"), dir.path()).unwrap();
        assert_eq!(got, "env-key");
    }

    #[test]
    fn resolve_key_from_falls_back_to_key_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STABILITY_KEY_FILE_NAME), "file-key\n").unwrap();
        let got = resolve_key_from(None, dir.path()).unwrap();
        assert_eq!(got, "file-key");
    }

    #[test]
    fn resolve_key_from_missing_both_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_key_from(None, dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "got: {err:#}");
    }
}
