//! Black Forest Labs' FLUX image API — kaibo's fifth media kind
//! (`ProviderKind::Bfl`), a sibling of `src/stability.rs` and `src/dashscope.rs`.
//! See `docs/bfl.md` for the spec this module implements.
//!
//! # One endpoint per operation, every one of them asynchronous
//!
//! Unlike Stability (mostly synchronous, five ops deferred) or DashScope/Gemini
//! (always synchronous), **every** BFL operation is asynchronous: a create POST
//! answers with `{id, polling_url, cost, input_mp, output_mp}` (`AsyncResponse`), and
//! the artifact comes from polling `polling_url` — a regional host that must be
//! dialled **verbatim**, never rebuilt from `base_url` — until its status leaves
//! `Pending`/`Reasoning`/`Generating`.
//!
//! # Polling is TLS-strict, and carries the credential
//!
//! `polling_url` is an address BFL chose, not one the operator configured — the same
//! trust shape [`crate::cas::fetch_artifact_bytes`] already takes for `result.sample`
//! (see [`crate::tls::artifact_fetch_client`]'s doc: "every property kaibo checked
//! before dialling is a promise only the first hop keeps"). It is worse here, because
//! the poll request also carries the `x-key` credential: a plain `http://`
//! `polling_url` would send that key in the clear, and reqwest strips the standard
//! `Authorization` header on a cross-host redirect but **not** a custom header like
//! `x-key` — so a redirecting `polling_url` would carry the key onward to whatever
//! host the redirect names. [`BflClient`] therefore polls through
//! [`crate::tls::artifact_fetch_client`] rather than the general client `create` uses:
//! `https_only(true)` refuses a plain-`http` `polling_url` before a single byte of the
//! request leaves (no connection is even attempted), and `Policy::none()` means a
//! redirect is never followed — it surfaces as its own refusal
//! ([`BflError::UnsafePollingUrl`]) instead of the key riding onward. `create` keeps
//! the general client: `base_url` is the operator's own configured endpoint, and
//! where the operator's endpoint redirects is the operator's business — exactly the
//! split [`crate::tls::artifact_fetch_client`]'s own doc draws between "an operator's
//! configured `base_url`" and "an address that arrives inside a provider's response".
//!
//! # The shape: inline-poll-then-defer, not strictly one or the other
//!
//! `docs/bfl.md` offers two shapes and asks which one lands: poll inline within the
//! request for a fast op (a concept-image caller wants bytes, not a handle), or
//! answer strictly `Deferred` like an async provider. FLUX generations are seconds-
//! fast, so [`BflImageModel::generate`] does both in sequence: it polls
//! [`INLINE_POLL_BUDGET`] worth of cadence right there in the call, and only when the
//! provider is still working after that budget does it fall back to
//! [`crate::media::MediaOutcome::Deferred`] — the existing job lane
//! (`server.rs`'s background task, or the CLI's own wait loop) picks up the exact same
//! `polling_url` from there via [`BflImageModel::poll`], which stays a single GET, no
//! sleep, exactly like every other kind's `poll`. This is not a third shape: it is the
//! first shape (inline, bounded) with the second shape (`Deferred`) as its own
//! documented fallback, which is what "within the request" already implies — a bound
//! must resolve to *something* once it is hit, and `Deferred` is the shape that
//! already exists for "still working, hand back a job".
//!
//! One more thing this shape buys for free: **`AsyncResponse.cost`** (credits,
//! `input_mp`, `output_mp`) is data the create call returns and nothing else ever
//! echoes back. On the common fast path it rides [`crate::media::MediaOutcome::Complete::note`]
//! (see [`cost_note`]) — the same channel #168 added for Gemini's commentary,
//! generalized here to "what the provider said about this call" rather than "what the
//! model said". On the rare slow path (fallen back to `Deferred`), the cost is not
//! carried forward — [`crate::media::MediaOutcome::Deferred`] and
//! [`crate::media::MediaPollOutcome::Complete`] have no note channel today. Recorded
//! here rather than silently accepted: a real per-artifact cost field in
//! `cas::Provenance`, mirroring `seed`, would close this gap for every media kind at
//! once, but it is a shared-seam change well outside a first BFL landing.
//!
//! # Which op runs
//!
//! The starter set is five named operations (`BFL_OPS`), one endpoint each. A
//! caller's `op` picks one directly; omitting `op` runs the cast's `image` slot model
//! id as the operation name — the same role Stability's model id plays for its
//! `generate` family (`GenerateRoute::classify`). Either way the name is checked
//! against `BFL_OPS`, so a typo'd slot model id and a typo'd `op` fail the same loud
//! way ([`BflError::UnknownOperation`]).
//!
//! # Webhooks are refused, not silently dropped
//!
//! `webhook_url`/`webhook_secret` ride `fields` like any other provider knob — except
//! kaibo is stdio-only and never binds a socket or receives a callback (see AGENTS.md).
//! Passing them through unchanged would have the provider answer
//! `AsyncWebhookResponse` (no `polling_url` at all) for money kaibo cannot collect, so
//! [`refuse_webhook_fields`] refuses both names before any request is sent.
//!
//! # Inputs and cost are wire-identical
//!
//! `MediaRequest.inputs` field names ride verbatim as JSON string keys
//! (`input_image`..`input_image_8`), base64-encoded — no field-name translation table
//! the way Gemini's `IMAGE_CONFIG_FIELDS` needs, because BFL's JSON body already uses
//! the same names `MediaInput::field` carries.
//!
//! # The artifact arrives as a signed, expiring URL
//!
//! `result.sample` on a `Ready` poll is a delivery link good for about ten minutes —
//! fetched immediately via [`crate::cas::fetch_artifact_bytes`] (the DashScope
//! precedent: https-only, no redirect followed, size-bounded), never stored as a
//! reference. The mime is read from the bytes themselves
//! ([`crate::view_image::sniff_mime`]), not trusted from a header, for the same reason
//! DashScope's `artifact_mime` does that: an object store commonly answers with the
//! wrong or a missing `Content-Type`, and the bytes cannot lie about themselves.

use std::time::Duration;

use anyhow::Result as AnyResult;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::media::{MediaArtifact, MediaJobId, MediaOutcome, MediaPollOutcome, MediaRequest};

/// BFL's API host. A root, not a route — every op appends its own path, the
/// client-appends-its-path contract every configurable base URL in kaibo follows.
pub const DEFAULT_BASE_URL: &str = "https://api.bfl.ai";

/// The env var that overrides the key-file.
pub const BFL_KEY_ENV_VAR: &str = "BFL_API_KEY";

/// The key-file's name within `$HOME`.
pub const BFL_KEY_FILE_NAME: &str = ".bfl-key";

/// How long [`BflImageModel::generate`] keeps polling in-call before handing back a
/// [`crate::media::MediaOutcome::Deferred`] job instead. FLUX generations are
/// documented as seconds-fast, so this is generous headroom for the common case, not
/// a measured ceiling — a real one wants a live probe once a key exists (see
/// `docs/bfl.md`'s testing section and this PR's report).
pub const INLINE_POLL_BUDGET: Duration = Duration::from_secs(30);

/// The cadence between in-call polls while under [`INLINE_POLL_BUDGET`].
pub const INLINE_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// The `fields` names refused outright rather than sent through — see the module
/// doc's "Webhooks are refused" section.
const WEBHOOK_FIELDS: &[&str] = &["webhook_url", "webhook_secret"];

/// One BFL operation this facade wires: its caller-facing name, its endpoint path,
/// and one line on why it is in the starter set — rendered into the `op` schema doc
/// verbatim, the #166 pattern (`Stability::OpSpec`, `docs/bfl.md`'s starter table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpSpec {
    pub name: &'static str,
    /// The path segment after the base URL, leading slash included.
    pub path: &'static str,
    pub why: &'static str,
}

/// The starter op table — five endpoints, confirmed against the live
/// `https://api.bfl.ai/openapi.json` (2026-08-31). See `docs/bfl.md` for the full
/// FLUX family this deliberately does not wire yet (`flux-tools/*`, finetunes,
/// video).
pub const BFL_OPS: &[OpSpec] = &[
    OpSpec {
        name: "flux-dev",
        path: "/v1/flux-dev",
        why: "the cheap rung — probing and drafts",
    },
    OpSpec {
        name: "flux-2-pro",
        path: "/v1/flux-2-pro",
        why: "the quality default",
    },
    OpSpec {
        name: "flux-2-flex",
        path: "/v1/flux-2-flex",
        why: "parameter-heavy control",
    },
    OpSpec {
        name: "flux-pro-1.1-ultra",
        path: "/v1/flux-pro-1.1-ultra",
        why: "high-resolution stills",
    },
    OpSpec {
        name: "flux-kontext-pro",
        path: "/v1/flux-kontext-pro",
        why: "image editing with reference inputs",
    },
];

/// Look up one operation by the name a caller (or a slot's model id) named.
pub fn op_by_name(name: &str) -> Option<&'static OpSpec> {
    BFL_OPS.iter().find(|o| o.name == name)
}

/// Every operation name this facade wires, in table order — for a schema enum or a
/// refusal, rendered from the table so it cannot drift from what [`op_by_name`]
/// accepts.
pub fn op_names() -> Vec<&'static str> {
    BFL_OPS.iter().map(|o| o.name).collect()
}

/// Which operation one call runs: the caller's `op` when given, else the cast's
/// `image` slot model id — the same role Stability's slot id plays for its `generate`
/// family. Both sources are checked against [`BFL_OPS`] the same way, by the caller of
/// this function.
pub fn resolve_op_name<'a>(request: &'a MediaRequest, slot_model: &'a str) -> &'a str {
    request.op.as_deref().unwrap_or(slot_model)
}

// --- Errors --------------------------------------------------------------------

/// Everything that can go wrong building or interpreting a BFL call. Its own type,
/// for the same reason its siblings have one: the pure functions below stay
/// unit-testable with no reqwest type in the loop.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BflError {
    /// The HTTP request itself failed (DNS, connect, TLS, a dropped connection).
    #[error("BFL request failed: {0}")]
    Transport(String),

    /// A non-2xx response from either the create call or a poll. `body` is BFL's
    /// `{detail: [...]}` validation shape rendered readably when it parses, the raw
    /// text otherwise.
    #[error("BFL returned {status}: {body}")]
    Provider { status: u16, body: String },

    /// A 2xx create response that is not `{id, polling_url, ...}` and not the
    /// webhook shape either — this module has no third shape to fall back to.
    #[error("BFL's create response didn't parse as {{id, polling_url, ...}}: {0}")]
    InvalidBody(String),

    /// A 2xx create response carrying `webhook_url` instead of `polling_url` — only
    /// reachable if [`refuse_webhook_fields`] were bypassed, since that is the one
    /// thing that shape needs to appear. Refused here as the structural backstop
    /// rather than assumed unreachable: this module has no polling_url to poll.
    #[error(
        "BFL answered with a webhook-shaped response (a `webhook_url`, no `polling_url`), \
         which kaibo has nothing to poll — kaibo never asks for a webhook, so seeing one \
         means the request carried `webhook_url`/`webhook_secret` some other way. Drop \
         those fields; kaibo polls the provider itself"
    )]
    UnexpectedWebhookResponse,

    /// A 2xx poll response that is not `{id, status, ...}`.
    #[error("BFL's poll response didn't parse as {{id, status, ...}}: {0}")]
    InvalidPollBody(String),

    /// `polling_url` — a provider-chosen address carrying the `x-key` credential on
    /// every poll — is not a plain `https` address, or its response redirected.
    /// Refused rather than dialled: see the module doc's "Polling is TLS-strict"
    /// section. This names BFL's own response, not a mistake in the caller's request.
    #[error(
        "BFL's poll response named an address kaibo will not dial: {polling_url:?} \
         ({reason}). kaibo's API key rides this request, so it only follows a plain \
         https link with no redirect — an unverified hop would carry the key onward. \
         This is BFL's own response, not a mistake in your request; retry the \
         generation"
    )]
    UnsafePollingUrl { polling_url: String, reason: String },

    /// The caller named an `op` (or the slot's model id resolved to one) that this
    /// facade does not wire. Refused rather than run as a guessed default: an
    /// unrelated FLUX variant returned for the wrong endpoint is a wrong answer that
    /// looks entirely like a right one, and it costs a paid generation to find out.
    #[error(
        "`op` {asked:?} is not an operation kaibo wires for BFL, so nothing was generated. \
         Pass one of: {}. Omit `op` to run the cast's image-slot model id as the \
         operation instead.",
        op_names().join(", ")
    )]
    UnknownOperation { asked: String },

    /// A `fields` entry named a webhook parameter — refused before any request is
    /// sent, so nothing is billed for a callback kaibo cannot receive.
    #[error(
        "`fields.{field}` asks BFL to call back a webhook, and kaibo is stdio-only — it \
         never binds a socket or receives a callback, so the notification would go \
         nowhere and the generation would still be paid for. Drop `fields.{field}`; \
         kaibo already polls the provider for you"
    )]
    WebhookNotSupported { field: &'static str },

    /// A `Ready` poll whose `result` carries no `sample` string — the one field this
    /// module actually needs from a successful result. Refused rather than treated
    /// as "no artifacts", since `Ready` is documented to always carry one.
    #[error(
        "BFL reported status Ready but the result carries no `sample` link to fetch — \
         kaibo has nothing to store. This is the provider answering outside its \
         documented shape; retry the generation"
    )]
    ReadyWithNoSample,

    /// `status: "Error"` — the provider ran the request and failed on it.
    #[error(
        "BFL reported a generation error: {detail}. The provider ran the request \
         and failed on its own side — retry the generation"
    )]
    GenerationError { detail: String },

    /// `status: "Request Moderated"` — refused before generation ran.
    #[error(
        "BFL refused this request before generating anything (Request Moderated): \
         {detail}. Change the prompt or the input images and try again"
    )]
    RequestModerated { detail: String },

    /// `status: "Content Moderated"` — the generation ran but its output was blocked.
    #[error(
        "BFL generated an image and then blocked it (Content Moderated): {detail}. \
         Change the prompt or the input images and try again"
    )]
    ContentModerated { detail: String },

    /// `status: "Task not found"` — the id has expired or was never valid on this
    /// host. Named separately from a generic provider error because the fix is
    /// different: there is no result to retry collecting, only a fresh generation.
    #[error(
        "BFL reports this task as not found — the id has expired or this polling_url \
         no longer resolves to a result. Generate again; a task id is not durable \
         across a long wait"
    )]
    TaskNotFound,

    /// A poll `status` outside the six documented values. Refused rather than
    /// silently treated as still-pending, the same discipline `StabilityError::UnknownMediaType`
    /// applies to an unrecognized content type: a seventh status is BFL shipping
    /// something this module has a real decision to make about.
    #[error(
        "BFL reported poll status {0:?}, which is none of the six this module \
         recognizes (Pending, Reasoning, Generating, Ready, Error, Request Moderated, \
         Content Moderated, Task not found) — refusing rather than guessing whether to \
         keep polling"
    )]
    UnknownStatus(String),

    /// Fetching `result.sample` failed. Pre-rendered from [`crate::cas::FetchError`],
    /// whose own messages name the fix (https-only, no redirects, a size ceiling).
    #[error("fetching the generated image: {0}")]
    Fetch(String),

    /// The fetched bytes are not one of the four formats the media store can name on
    /// disk (png/jpeg/webp/gif) — read from the bytes themselves, not trusted from a
    /// header. See the module doc's "signed, expiring URL" section.
    #[error(
        "the fetched image is not png, jpeg, webp, or gif — the bytes match none of \
         them, and the server called it {got:?}. kaibo stores an artifact under the \
         format its bytes actually are, so there is nothing to store here"
    )]
    UnusableContentType { got: Option<String> },
}

fn provider_error(status: u16, body: &[u8]) -> BflError {
    let text = String::from_utf8_lossy(body);
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(rendered) = render_validation_detail(&value) {
            return BflError::Provider {
                status,
                body: rendered,
            };
        }
    }
    BflError::Provider {
        status,
        body: text.trim().to_string(),
    }
}

/// Render BFL's `HTTPValidationError` shape (`{"detail": [{"loc": [...], "msg": ...}]}`)
/// readably, when the body is that shape. `None` for anything else, so the caller
/// falls back to the raw text rather than a `null` string.
fn render_validation_detail(value: &Value) -> Option<String> {
    let detail = value.get("detail")?;
    if let Some(msg) = detail.as_str() {
        return Some(msg.to_string());
    }
    let items = detail.as_array()?;
    let rendered: Vec<String> = items
        .iter()
        .filter_map(|item| {
            let loc = item
                .get("loc")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .map(|part| match part {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            let msg = item.get("msg").and_then(Value::as_str).unwrap_or("");
            (!msg.is_empty()).then(|| format!("{loc}: {msg}"))
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join("; "))
}

// --- Request -------------------------------------------------------------------

/// Refuse a request naming a webhook field — see the module doc's "Webhooks are
/// refused" section. Checked before any request is built.
pub fn refuse_webhook_fields(request: &MediaRequest) -> Result<(), BflError> {
    for field in WEBHOOK_FIELDS {
        if request.fields.iter().any(|(name, _)| name == field) {
            return Err(BflError::WebhookNotSupported { field });
        }
    }
    Ok(())
}

/// Build the JSON request body for one operation. Pure: no network, no clock.
///
/// `prompt` leads, then every named input rides verbatim as a base64 string under
/// its own field name (`input_image`, `input_image_2`, ...), then every caller field
/// passes through with its stated JSON type. A `fields` entry sharing a name with an
/// input part replaces it — the same later-entry-wins rule `Stability::build_form_fields`
/// already uses — which is a caller's own mistake to make, not one kaibo prevents:
/// `fields` is typed scalars only, so overwriting an image with one produces a
/// provider 400, not silent corruption.
pub fn build_request_body(request: &MediaRequest) -> Value {
    let mut body = Map::new();
    body.insert("prompt".to_string(), Value::String(request.prompt.clone()));
    for input in &request.inputs {
        body.insert(
            input.field.clone(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(&input.bytes)),
        );
    }
    for (name, value) in &request.fields {
        body.insert(name.clone(), value.to_json());
    }
    Value::Object(body)
}

// --- Create response -------------------------------------------------------------

/// The parsed `AsyncResponse` — the create call's success shape.
#[derive(Debug, Clone, PartialEq)]
pub struct AsyncResponse {
    pub id: String,
    pub polling_url: String,
    /// Credits, verbatim — kaibo converts nothing (native-unit ruling, 2026-08-22).
    pub cost: Option<f64>,
    pub input_mp: Option<f64>,
    pub output_mp: Option<f64>,
}

#[derive(Deserialize)]
struct RawAsyncResponse {
    id: Option<String>,
    polling_url: Option<String>,
    cost: Option<f64>,
    input_mp: Option<f64>,
    output_mp: Option<f64>,
    /// Present only on the webhook-shaped sibling response — see
    /// [`BflError::UnexpectedWebhookResponse`].
    webhook_url: Option<String>,
}

/// Interpret one create-call HTTP response. Pure.
pub fn parse_create_response(status: u16, body: &[u8]) -> Result<AsyncResponse, BflError> {
    if !(200..300).contains(&status) {
        return Err(provider_error(status, body));
    }
    let raw: RawAsyncResponse =
        serde_json::from_slice(body).map_err(|e| BflError::InvalidBody(e.to_string()))?;
    match raw.polling_url {
        Some(polling_url) => Ok(AsyncResponse {
            id: raw.id.unwrap_or_default(),
            polling_url,
            cost: raw.cost,
            input_mp: raw.input_mp,
            output_mp: raw.output_mp,
        }),
        None if raw.webhook_url.is_some() => Err(BflError::UnexpectedWebhookResponse),
        None => Err(BflError::InvalidBody(
            "the response has neither `polling_url` nor `webhook_url`".to_string(),
        )),
    }
}

/// What the provider said about one call's cost, for
/// [`crate::media::MediaOutcome::Complete::note`] — see the module doc's "cost" section.
/// `None` when the create response carried none of the three fields, so a bare
/// artifact list is not followed by an empty line.
pub fn cost_note(created: &AsyncResponse) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(cost) = created.cost {
        parts.push(format!("{cost} credits"));
    }
    if let Some(mp) = created.input_mp {
        parts.push(format!("{mp} input MP"));
    }
    if let Some(mp) = created.output_mp {
        parts.push(format!("{mp} output MP"));
    }
    (!parts.is_empty()).then(|| format!("BFL reports this request cost {}.", parts.join(", ")))
}

// --- Poll response ---------------------------------------------------------------

/// One poll's outcome, before the artifact is fetched.
#[derive(Debug, Clone, PartialEq)]
pub enum PollDecision {
    Pending,
    Ready { sample_url: String },
}

#[derive(Deserialize)]
struct RawResultResponse {
    status: String,
    result: Option<Value>,
    details: Option<Value>,
}

/// Render whatever detail a terminal status carried, for the error variants that
/// need one. Prefers `details`, falls back to `result`, and never returns an empty
/// string — BFL's own words when it has them, "(no detail given)" when it doesn't.
fn render_detail(raw: &RawResultResponse) -> String {
    let carried = raw.details.as_ref().or(raw.result.as_ref());
    match carried {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(other) => other.to_string(),
        None => "(no detail given)".to_string(),
    }
}

/// Interpret one poll-call HTTP response. Pure.
pub fn parse_poll_response(status: u16, body: &[u8]) -> Result<PollDecision, BflError> {
    if !(200..300).contains(&status) {
        return Err(provider_error(status, body));
    }
    let raw: RawResultResponse =
        serde_json::from_slice(body).map_err(|e| BflError::InvalidPollBody(e.to_string()))?;
    match raw.status.as_str() {
        "Pending" | "Reasoning" | "Generating" => Ok(PollDecision::Pending),
        "Ready" => {
            let sample = raw
                .result
                .as_ref()
                .and_then(|r| r.get("sample"))
                .and_then(Value::as_str)
                .ok_or(BflError::ReadyWithNoSample)?;
            Ok(PollDecision::Ready {
                sample_url: sample.to_string(),
            })
        }
        "Error" => Err(BflError::GenerationError {
            detail: render_detail(&raw),
        }),
        "Request Moderated" => Err(BflError::RequestModerated {
            detail: render_detail(&raw),
        }),
        "Content Moderated" => Err(BflError::ContentModerated {
            detail: render_detail(&raw),
        }),
        "Task not found" => Err(BflError::TaskNotFound),
        other => Err(BflError::UnknownStatus(other.to_string())),
    }
}

/// The mime for one fetched artifact, read from the bytes — see the module doc.
fn artifact_mime(content_type: Option<&str>, bytes: &[u8]) -> Result<String, BflError> {
    match crate::view_image::sniff_mime(bytes) {
        Some(mime) => Ok(mime.to_string()),
        None => Err(BflError::UnusableContentType {
            got: content_type.map(|raw| raw.split(';').next().unwrap_or(raw).trim().to_string()),
        }),
    }
}

// --- The HTTP client -------------------------------------------------------------

/// A configured BFL connection: credential, base URL, and two HTTPS clients built
/// through kaibo's TLS seam — one per trust shape. See the module doc's "Polling is
/// TLS-strict" section for why the poll leg does not share `create`'s client.
#[derive(Clone)]
pub struct BflClient {
    http: reqwest::Client,
    /// The poll leg's own client — [`crate::tls::artifact_fetch_client`], the same
    /// posture [`crate::cas::fetch_artifact_bytes`] takes for `result.sample`:
    /// `polling_url` is a provider-chosen address carrying the `x-key` credential,
    /// so it gets `https_only(true)` and no redirects rather than `create`'s general
    /// client.
    poll_http: reqwest::Client,
    api_key: String,
    base_url: String,
    request_timeout: Duration,
}

impl std::fmt::Debug for BflClient {
    /// Manual: never render the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BflClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl BflClient {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        request_timeout: Duration,
    ) -> AnyResult<Self> {
        Ok(Self {
            http: crate::tls::https_client(request_timeout)?,
            poll_http: crate::tls::artifact_fetch_client(request_timeout)?,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            request_timeout,
        })
    }

    /// `POST {base}{op.path}`, authenticated with `x-key`.
    async fn create(&self, op: &OpSpec, request: &MediaRequest) -> Result<AsyncResponse, BflError> {
        let url = format!("{}{}", self.base_url, op.path);
        let resp = self
            .http
            .post(&url)
            .header("x-key", &self.api_key)
            .json(&build_request_body(request))
            .send()
            .await
            .map_err(|e| BflError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BflError::Transport(e.to_string()))?;
        parse_create_response(status, &bytes)
    }

    /// `GET polling_url` **verbatim** — never rebuilt from `base_url` — over the
    /// TLS-strict poll client. See the module doc's "Polling is TLS-strict" section:
    /// `polling_url` is a provider-chosen address that carries the `x-key`
    /// credential, so it is checked before dialling (a plain `http` address is
    /// refused with zero connection attempts) and again after the first hop answers
    /// (a redirect is refused rather than followed, since `Policy::none()` hands
    /// back the 3xx response itself rather than an error).
    async fn poll_once(&self, polling_url: &str) -> Result<PollDecision, BflError> {
        if !polling_url.starts_with("https://") {
            return Err(BflError::UnsafePollingUrl {
                polling_url: polling_url.to_string(),
                reason: "not an https address".to_string(),
            });
        }
        let resp = self
            .poll_http
            .get(polling_url)
            .header("x-key", &self.api_key)
            .send()
            .await
            .map_err(|e| BflError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(BflError::UnsafePollingUrl {
                polling_url: polling_url.to_string(),
                reason: format!(
                    "the response redirected ({status}), and a redirect is never followed"
                ),
            });
        }
        let status = status.as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BflError::Transport(e.to_string()))?;
        parse_poll_response(status, &bytes)
    }

    /// Fetch a `Ready` poll's `result.sample` and shape it into a [`MediaArtifact`].
    async fn fetch_artifact(&self, sample_url: &str) -> Result<MediaArtifact, BflError> {
        let (bytes, content_type) =
            crate::cas::fetch_artifact_bytes(sample_url, self.request_timeout)
                .await
                .map_err(|e| BflError::Fetch(e.to_string()))?;
        let mime = artifact_mime(content_type.as_deref(), &bytes)?;
        Ok(MediaArtifact {
            bytes,
            mime,
            // BFL's poll response reports no seed even when the request pinned one —
            // a caller's own `seed` field rides `fields` and is the reproduction
            // handle, the same posture DashScope's artifact takes.
            seed: None,
        })
    }
}

/// One BFL image model, bound to a cast's `image` slot: the client, the slot's model
/// id (the default operation — see [`resolve_op_name`]), and the inline-poll cadence.
#[derive(Clone)]
pub struct BflImageModel {
    client: BflClient,
    model: String,
    inline_poll_budget: Duration,
    inline_poll_interval: Duration,
}

impl BflImageModel {
    /// Bind a client to one model id — the shape `MediaArm::from_slot` builds.
    pub fn from_parts(client: &BflClient, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
            inline_poll_budget: INLINE_POLL_BUDGET,
            inline_poll_interval: INLINE_POLL_INTERVAL,
        }
    }

    /// Override the inline-poll cadence. Meant for tests exercising the
    /// falls-back-to-`Deferred` path, which would otherwise need real wall-clock
    /// time to reach — the same "overridable for tests" posture
    /// `StabilityClient::new`'s `base_url` parameter already has.
    pub fn with_inline_poll_timing(mut self, budget: Duration, interval: Duration) -> Self {
        self.inline_poll_budget = budget;
        self.inline_poll_interval = interval;
        self
    }
}

/// What the inline poll loop in [`BflImageModel::generate`] does with one poll
/// outcome, given whether the in-call budget is already spent. Pure and unit-tested
/// directly, extracted for exactly one reason: once the poll leg is TLS-strict (see
/// [`BflClient::poll_once`]), an offline transport test can no longer drive the loop
/// over a live "still pending" poll target, so the budget-exhausted branch needs a
/// seam that does not require a socket at all.
#[derive(Debug, Clone, PartialEq)]
enum InlineStep {
    /// Keep polling: sleep [`INLINE_POLL_INTERVAL`], then poll again.
    KeepPolling,
    /// The provider is done; fetch this URL and resolve.
    Ready { sample_url: String },
    /// Still `Pending` past the budget: hand back a job carrying the exact
    /// `polling_url` that was being polled — see the module doc's "shape" section.
    Defer(MediaJobId),
}

fn inline_step(decision: PollDecision, budget_spent: bool, polling_url: &str) -> InlineStep {
    match decision {
        PollDecision::Ready { sample_url } => InlineStep::Ready { sample_url },
        PollDecision::Pending if budget_spent => {
            InlineStep::Defer(MediaJobId(polling_url.to_string()))
        }
        PollDecision::Pending => InlineStep::KeepPolling,
    }
}

#[async_trait::async_trait]
impl crate::media::MediaModel for BflImageModel {
    /// Every op in [`BFL_OPS`] takes `input_image`..`input_image_8` — an edit is an
    /// image plus the instruction that refers to it.
    fn accepts_inputs(&self) -> bool {
        true
    }

    /// Five named endpoints behind one slot — see the module doc's "Which op runs".
    fn accepts_ops(&self) -> bool {
        true
    }

    async fn generate(&self, request: &MediaRequest) -> AnyResult<MediaOutcome> {
        refuse_webhook_fields(request)?;
        let op_name = resolve_op_name(request, &self.model);
        let op = op_by_name(op_name).ok_or_else(|| BflError::UnknownOperation {
            asked: op_name.to_string(),
        })?;
        let created = self.client.create(op, request).await?;
        let note = cost_note(&created);
        let started = tokio::time::Instant::now();
        loop {
            let decision = self.client.poll_once(&created.polling_url).await?;
            let budget_spent = started.elapsed() >= self.inline_poll_budget;
            match inline_step(decision, budget_spent, &created.polling_url) {
                InlineStep::Ready { sample_url } => {
                    let artifact = self.client.fetch_artifact(&sample_url).await?;
                    return Ok(MediaOutcome::Complete {
                        artifacts: vec![artifact],
                        note,
                    });
                }
                InlineStep::Defer(job) => {
                    // Still working after the in-call budget: hand back the same
                    // polling_url as a job the caller's own cadence (the background
                    // job lane, or the CLI's wait loop) continues from `poll` — a
                    // single GET, no sleep, same as every other deferred kind.
                    return Ok(MediaOutcome::Deferred(job));
                }
                InlineStep::KeepPolling => {
                    tokio::time::sleep(self.inline_poll_interval).await;
                }
            }
        }
    }

    /// Collect one deferred job: a single GET on the `polling_url` this job id
    /// carries, no sleep — the caller (the background job lane, or the CLI's own
    /// loop) owns the cadence, exactly like `StabilityClient::poll`.
    async fn poll(&self, job: &MediaJobId) -> AnyResult<MediaPollOutcome> {
        match self.client.poll_once(&job.0).await? {
            PollDecision::Pending => Ok(MediaPollOutcome::Pending),
            PollDecision::Ready { sample_url } => {
                let artifact = self.client.fetch_artifact(&sample_url).await?;
                Ok(MediaPollOutcome::Complete(vec![artifact]))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{FieldValue, MediaInput};

    fn request(prompt: &str) -> MediaRequest {
        MediaRequest {
            prompt: prompt.to_string(),
            ..Default::default()
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    // --- op table ---------------------------------------------------------------

    #[test]
    fn every_starter_op_resolves_by_name() {
        for name in [
            "flux-dev",
            "flux-2-pro",
            "flux-2-flex",
            "flux-pro-1.1-ultra",
            "flux-kontext-pro",
        ] {
            let op = op_by_name(name).unwrap_or_else(|| panic!("{name} must be wired"));
            assert_eq!(op.name, name);
            assert!(op.path.starts_with("/v1/"));
        }
        assert_eq!(op_names().len(), 5);
    }

    #[test]
    fn an_unwired_op_name_resolves_to_nothing() {
        assert!(
            op_by_name("flux-3-video").is_none(),
            "out of scope per docs/bfl.md"
        );
    }

    #[test]
    fn resolve_op_name_prefers_the_callers_op_over_the_slot_model() {
        let mut r = request("a lighthouse");
        r.op = Some("flux-dev".to_string());
        assert_eq!(resolve_op_name(&r, "flux-2-pro"), "flux-dev");
    }

    #[test]
    fn resolve_op_name_falls_back_to_the_slot_model_when_op_is_omitted() {
        let r = request("a lighthouse");
        assert_eq!(resolve_op_name(&r, "flux-2-pro"), "flux-2-pro");
    }

    // --- request building ---------------------------------------------------------

    #[test]
    fn the_prompt_and_typed_fields_ride_the_body_verbatim() {
        let mut r = request("a red cube");
        r.fields = vec![
            ("seed".to_string(), FieldValue::Num(42.into())),
            ("output_format".to_string(), FieldValue::Str("png".into())),
            ("disable_pup".to_string(), FieldValue::Bool(true)),
        ];
        let body = build_request_body(&r);
        assert_eq!(body["prompt"], "a red cube");
        assert_eq!(body["seed"], 42);
        assert!(
            body["seed"].is_i64(),
            "an integer must reach the wire as one"
        );
        assert_eq!(body["output_format"], "png");
        assert_eq!(body["disable_pup"], true);
    }

    #[test]
    fn named_inputs_ride_the_body_as_base64_under_their_own_field_name() {
        let mut r = request("edit this");
        r.inputs = vec![MediaInput::new(
            "input_image",
            crate::cas::Extension::Png,
            b"\x89PNG\r\n\x1a\ncat".to_vec(),
        )];
        let body = build_request_body(&r);
        assert_eq!(
            body["input_image"],
            b64(b"\x89PNG\r\n\x1a\ncat"),
            "the field name in `inputs` is the JSON key verbatim, per docs/bfl.md"
        );
    }

    #[test]
    fn multiple_named_inputs_each_land_under_their_own_field() {
        let mut r = request("multiref edit");
        r.inputs = vec![
            MediaInput::new("input_image", crate::cas::Extension::Png, b"one".to_vec()),
            MediaInput::new("input_image_2", crate::cas::Extension::Png, b"two".to_vec()),
        ];
        let body = build_request_body(&r);
        assert_eq!(body["input_image"], b64(b"one"));
        assert_eq!(body["input_image_2"], b64(b"two"));
    }

    // --- webhook refusal ---------------------------------------------------------

    #[test]
    fn a_webhook_url_field_is_refused_before_any_request_is_built() {
        let mut r = request("a red cube");
        r.fields = vec![(
            "webhook_url".to_string(),
            FieldValue::Str("https://example.test/hook".into()),
        )];
        let err = refuse_webhook_fields(&r).expect_err("must refuse");
        assert!(matches!(
            err,
            BflError::WebhookNotSupported {
                field: "webhook_url"
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("stdio-only"), "names why: {msg}");
        assert!(
            msg.contains("Drop `fields.webhook_url`"),
            "names the fix: {msg}"
        );
    }

    #[test]
    fn a_webhook_secret_field_is_also_refused() {
        let mut r = request("a red cube");
        r.fields = vec![("webhook_secret".to_string(), FieldValue::Str("s".into()))];
        assert!(refuse_webhook_fields(&r).is_err());
    }

    #[test]
    fn a_request_with_no_webhook_fields_passes() {
        let r = request("a red cube");
        assert!(refuse_webhook_fields(&r).is_ok());
    }

    // --- create response ----------------------------------------------------------

    #[test]
    fn a_successful_create_response_parses_every_field() {
        let body = serde_json::json!({
            "id": "t-1",
            "polling_url": "https://api.us1.bfl.ai/v1/get_result?id=t-1",
            "cost": 0.05,
            "input_mp": 1.05,
            "output_mp": 1.05,
        })
        .to_string();
        let resp = parse_create_response(200, body.as_bytes()).expect("parses");
        assert_eq!(resp.id, "t-1");
        assert_eq!(
            resp.polling_url,
            "https://api.us1.bfl.ai/v1/get_result?id=t-1"
        );
        assert_eq!(resp.cost, Some(0.05));
        assert_eq!(resp.input_mp, Some(1.05));
        assert_eq!(resp.output_mp, Some(1.05));
    }

    #[test]
    fn a_webhook_shaped_response_is_refused_as_unexpected() {
        let body = serde_json::json!({
            "id": "t-1",
            "status": "Queued",
            "webhook_url": "https://example.test/hook",
        })
        .to_string();
        let err = parse_create_response(200, body.as_bytes()).expect_err("no polling_url");
        assert_eq!(err, BflError::UnexpectedWebhookResponse);
    }

    #[test]
    fn a_422_validation_error_renders_its_field_and_message() {
        let body = serde_json::json!({
            "detail": [{"loc": ["body", "width"], "msg": "Input should be a multiple of 32", "type": "value_error"}]
        })
        .to_string();
        let err = parse_create_response(422, body.as_bytes()).expect_err("422");
        let msg = err.to_string();
        assert!(msg.contains("422"), "{msg}");
        assert!(msg.contains("body.width"), "{msg}");
        assert!(msg.contains("multiple of 32"), "{msg}");
    }

    #[test]
    fn a_non_json_error_body_keeps_its_raw_text() {
        let err = parse_create_response(500, b"upstream timeout").expect_err("500");
        let msg = err.to_string();
        assert!(
            msg.contains("500") && msg.contains("upstream timeout"),
            "{msg}"
        );
    }

    #[test]
    fn an_unparseable_2xx_body_is_refused() {
        let err = parse_create_response(200, b"not json at all").expect_err("garbage 2xx");
        assert!(matches!(err, BflError::InvalidBody(_)));
    }

    #[test]
    fn cost_note_renders_every_reported_figure() {
        let resp = AsyncResponse {
            id: "t".into(),
            polling_url: "https://x/y".into(),
            cost: Some(0.05),
            input_mp: Some(1.0),
            output_mp: Some(1.05),
        };
        let note = cost_note(&resp).expect("some figure was reported");
        assert!(note.contains("0.05 credits"), "{note}");
        assert!(note.contains("1 input MP"), "{note}");
        assert!(note.contains("1.05 output MP"), "{note}");
    }

    #[test]
    fn cost_note_is_absent_when_nothing_was_reported() {
        let resp = AsyncResponse {
            id: "t".into(),
            polling_url: "https://x/y".into(),
            cost: None,
            input_mp: None,
            output_mp: None,
        };
        assert_eq!(cost_note(&resp), None);
    }

    // --- poll response --------------------------------------------------------------

    #[test]
    fn pending_reasoning_and_generating_all_mean_keep_polling() {
        for status in ["Pending", "Reasoning", "Generating"] {
            let body = serde_json::json!({"id": "t", "status": status}).to_string();
            assert_eq!(
                parse_poll_response(200, body.as_bytes()).unwrap(),
                PollDecision::Pending,
                "status {status} must mean keep polling"
            );
        }
    }

    #[test]
    fn ready_carries_the_sample_url_out() {
        let body = serde_json::json!({
            "id": "t",
            "status": "Ready",
            "result": {"sample": "https://delivery.bfl.ai/results/t/0.png"},
        })
        .to_string();
        assert_eq!(
            parse_poll_response(200, body.as_bytes()).unwrap(),
            PollDecision::Ready {
                sample_url: "https://delivery.bfl.ai/results/t/0.png".to_string()
            }
        );
    }

    #[test]
    fn ready_with_no_sample_is_refused_rather_than_treated_as_empty() {
        let body = serde_json::json!({"id": "t", "status": "Ready", "result": {}}).to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("no sample");
        assert_eq!(err, BflError::ReadyWithNoSample);
    }

    #[test]
    fn error_status_carries_the_providers_detail() {
        let body = serde_json::json!({
            "id": "t",
            "status": "Error",
            "details": {"reason": "internal failure"},
        })
        .to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("Error status");
        assert!(
            matches!(err, BflError::GenerationError { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("internal failure"), "{err}");
    }

    #[test]
    fn request_moderated_names_what_to_change() {
        let body = serde_json::json!({
            "id": "t",
            "status": "Request Moderated",
            "details": {"reason": "flagged prompt"},
        })
        .to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("moderated");
        assert!(matches!(err, BflError::RequestModerated { .. }));
        let msg = err.to_string();
        assert!(msg.contains("flagged prompt"), "{msg}");
        assert!(msg.contains("Change the prompt"), "{msg}");
    }

    #[test]
    fn content_moderated_is_distinct_from_request_moderated() {
        let body = serde_json::json!({"id": "t", "status": "Content Moderated"}).to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("moderated");
        assert!(matches!(err, BflError::ContentModerated { .. }));
        assert!(err.to_string().contains("blocked it"));
    }

    #[test]
    fn task_not_found_says_to_generate_again_not_retry_the_poll() {
        let body = serde_json::json!({"id": "t", "status": "Task not found"}).to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("expired");
        assert_eq!(err, BflError::TaskNotFound);
        assert!(err.to_string().contains("Generate again"));
    }

    #[test]
    fn an_unrecognized_status_is_refused_rather_than_treated_as_pending() {
        let body = serde_json::json!({"id": "t", "status": "Queued"}).to_string();
        let err = parse_poll_response(200, body.as_bytes()).expect_err("unknown status");
        assert_eq!(err, BflError::UnknownStatus("Queued".to_string()));
    }

    #[test]
    fn a_non_2xx_poll_is_a_provider_error() {
        let err = parse_poll_response(404, b"{}").expect_err("404");
        assert!(matches!(err, BflError::Provider { status: 404, .. }));
    }

    // --- polling_url safety ----------------------------------------------------------

    /// The scheme check fires with no client and no network at all: a plaintext
    /// `polling_url` is refused before `BflClient` ever touches the socket. See the
    /// module doc's "Polling is TLS-strict" section — the real-socket proof that
    /// zero connections happen lives in `tests/bfl_transport.rs`.
    #[tokio::test]
    async fn poll_once_refuses_a_plaintext_polling_url_without_dialing() {
        let client = BflClient::new("k", "https://example.test", Duration::from_secs(5)).unwrap();
        let err = client
            .poll_once("http://127.0.0.1:1/get_result?id=t-1")
            .await
            .expect_err("plaintext must be refused");
        match err {
            BflError::UnsafePollingUrl {
                polling_url,
                reason,
            } => {
                assert_eq!(polling_url, "http://127.0.0.1:1/get_result?id=t-1");
                assert!(reason.contains("https"), "{reason}");
            }
            other => panic!("expected UnsafePollingUrl, got {other:?}"),
        }
    }

    /// The refusal names what was refused, why (the credential rides this request),
    /// and that it is BFL's own response rather than a mistake in the caller's
    /// request — the house rule for a refusal string.
    #[test]
    fn unsafe_polling_url_names_what_why_and_whose_mistake_it_is_not() {
        let err = BflError::UnsafePollingUrl {
            polling_url: "http://evil.example/x".to_string(),
            reason: "not an https address".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("http://evil.example/x"),
            "names the url: {msg}"
        );
        assert!(
            msg.contains("API key rides this request"),
            "names why: {msg}"
        );
        assert!(
            msg.contains("not a mistake in your request"),
            "says whose fault it is: {msg}"
        );
    }

    // --- inline poll decision ----------------------------------------------------------

    /// A `Ready` decision resolves regardless of the budget — the artifact is in
    /// hand, so there is nothing left to defer.
    #[test]
    fn ready_resolves_even_with_the_budget_already_spent() {
        let step = inline_step(
            PollDecision::Ready {
                sample_url: "https://delivery.bfl.ai/0.png".to_string(),
            },
            true,
            "https://api.us1.bfl.ai/v1/get_result?id=t-1",
        );
        assert_eq!(
            step,
            InlineStep::Ready {
                sample_url: "https://delivery.bfl.ai/0.png".to_string()
            }
        );
    }

    /// Still pending and the budget isn't spent yet: keep polling.
    #[test]
    fn pending_before_the_budget_keeps_polling() {
        let step = inline_step(
            PollDecision::Pending,
            false,
            "https://api.us1.bfl.ai/v1/get_result?id=t-1",
        );
        assert_eq!(step, InlineStep::KeepPolling);
    }

    /// **The decision this extraction exists for.** Still pending once the in-call
    /// budget is spent: defer, and the job carries the *exact* `polling_url` that
    /// was being polled — the caller's own cadence (the background job lane, or the
    /// CLI's wait loop) has to reach the same address, not one reconstructed later.
    #[test]
    fn pending_past_the_budget_defers_with_the_identical_polling_url() {
        let polling_url = "https://api.us1.bfl.ai/v1/get_result?id=t-1";
        let step = inline_step(PollDecision::Pending, true, polling_url);
        assert_eq!(step, InlineStep::Defer(MediaJobId(polling_url.to_string())));
    }

    // --- mime sniffing --------------------------------------------------------------

    #[test]
    fn a_real_png_is_recognized_from_its_bytes() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(artifact_mime(Some("image/png"), &png).unwrap(), "image/png");
    }

    #[test]
    fn unrecognizable_bytes_are_refused_naming_the_claimed_type() {
        let err = artifact_mime(Some("application/xml"), b"<Error/>").expect_err("not an image");
        assert_eq!(
            err,
            BflError::UnusableContentType {
                got: Some("application/xml".to_string())
            }
        );
    }

    // --- unknown operation ------------------------------------------------------------

    #[test]
    fn unknown_operation_names_the_ask_and_the_valid_list() {
        let err = BflError::UnknownOperation {
            asked: "flux-3-video".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("flux-3-video"), "{msg}");
        assert!(msg.contains("flux-2-pro"), "{msg}");
        assert!(msg.contains("Omit `op`"), "{msg}");
    }
}
