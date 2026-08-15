//! Alibaba DashScope's multimodal-generation route — kaibo's third media kind
//! (`ProviderKind::DashScope`), a sibling of `src/stability.rs` and
//! `src/openai_images.rs`.
//!
//! # One route, the `wan` image family
//!
//! `POST {base}/api/v1/services/aigc/multimodal-generation/generation` generates
//! images from text. It is **synchronous**: the POST answers with the artifacts, so
//! [`crate::media::MediaModel::generate`] always resolves to
//! [`crate::media::MediaOutcome::Complete`] and `poll` is unreachable (it bails
//! loudly, as OpenAI Images' does).
//!
//! DashScope also documents an *asynchronous* text-to-image route
//! (`/api/v1/services/aigc/text2image/image-synthesis`, submit-then-poll behind an
//! `X-DashScope-Async` header). kaibo does not speak it. The synchronous route
//! reaches the same models, and the async one is an account entitlement not every
//! subscription carries — measured 2026-08-15, a subscription that generates happily
//! on this route answers the async one with `current user api does not support
//! asynchronous calls`. If a deferred DashScope operation ever matters, the seam is
//! ready for it ([`crate::media::MediaOutcome::Deferred`]); this module simply has no
//! deferred shape today.
//!
//! # Text belongs on the `openai` kind
//!
//! DashScope hosts serve text through an OpenAI-compatible endpoint
//! (`/compatible-mode/v1`), which kaibo already speaks. So this kind is media-only:
//! a `wan` model on an `image` slot here, and any text model on a `kind = "openai"`
//! backend pointed at the same host. One account, two stanzas, each on the wire it
//! actually speaks.
//!
//! # Artifacts arrive as URLs — the departure from every other media kind
//!
//! The response carries a presigned link per image, not inline bytes. That is the
//! opposite of what `src/openai_images.rs` requires of its own provider, and the
//! reasoning is not that the trust question went away — it is that the two cases are
//! different. There, base64 is available and asking for a URL spends credits on
//! artifacts kaibo would then refuse; the refusal costs nothing and prevents waste.
//! Here URLs are the only delivery mechanism, so refusing them means the operator
//! fetches the link themselves with a raw key — moving the same network hop out of
//! kaibo and into a shell, with the credential now in a command line. And a
//! content-addressed store cannot address what it has not read: the digest *is* the
//! address, so the bytes are the artifact's existence condition, not a convenience.
//!
//! The fetch itself lives in [`crate::cas::fetch_artifact_bytes`], beside the store
//! rather than in this module, so the bound and the refusals are shared with whatever
//! URL-delivering provider lands next. Its `Content-Type` is what names the mime —
//! see [`artifact_mime`].
//!
//! # Fields ride verbatim, typed by the caller
//!
//! Same passthrough posture as the sibling kinds: `n`, `size`, `seed`,
//! `negative_prompt`, ... ride [`crate::media::MediaRequest::fields`] into the
//! request's `parameters` object with their stated JSON type, and DashScope answers
//! for its own knobs. Two provider-side notes worth knowing, both measured:
//!
//! - `enable_interleave` is **seeded true** by [`build_request_body`], because a
//!   text-only request is refused without it (`the last message must contain 1 to 4
//!   images`). A caller field of the same name replaces the seed, the same merge
//!   Stability's `output_format` seed uses.
//! - DashScope **silently ignores unknown parameter names** — a misspelled knob is a
//!   no-op, not a 400. kaibo therefore sends only what a caller actually wrote and
//!   never leans on the provider to catch a typo.
//!
//! # Credentials
//!
//! Its own `DASHSCOPE_API_KEY` / `~/.dashscope-key`, not shared with the `openai`
//! kind: an operator running both wires against one DashScope account wants one name
//! per account, not one per protocol. The key is required — this kind has no keyless
//! target.

use std::time::Duration;

use anyhow::Result as AnyResult;
use reqwest::header::AUTHORIZATION;
use serde_json::{json, Map, Value};

use crate::media::{MediaArtifact, MediaJobId, MediaOutcome, MediaPollOutcome, MediaRequest};

/// The environment variable holding a DashScope key.
pub const DASHSCOPE_KEY_ENV_VAR: &str = "DASHSCOPE_API_KEY";

/// The key-file consulted when the environment is unset.
pub const DASHSCOPE_KEY_FILE_NAME: &str = ".dashscope-key";

/// DashScope's international API root. A dedicated-endpoint subscription has its own
/// host and sets `base_url`; this is the shared-endpoint default. A root, not a
/// route — [`generation_url`] appends the path, the client-appends-its-path contract
/// every configurable base URL in kaibo follows.
pub const DASHSCOPE_API_BASE: &str = "https://dashscope-intl.aliyuncs.com";

/// The synchronous multimodal-generation route, appended to the base URL.
pub const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// The parameter that lets a text-only prompt through. Without it the model demands
/// one to four input images.
const ENABLE_INTERLEAVE: &str = "enable_interleave";

/// The full generation URL for a base. Tolerates a trailing slash on the base so an
/// operator's `base_url` spelling never changes the route.
pub fn generation_url(base_url: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), GENERATION_PATH)
}

// --- Errors ------------------------------------------------------------------

/// Everything that can go wrong building or interpreting a DashScope generation.
/// Own type for the same reason its siblings have one: [`build_request_body`] and
/// [`parse_response`] stay pure and unit-testable with no reqwest types in the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashScopeError {
    /// The HTTP request itself failed (DNS, connect, TLS, a dropped connection) —
    /// message pre-rendered so this stays `Clone`/`PartialEq`.
    Transport(String),
    /// A non-2xx response. `body` is DashScope's `{code, message, request_id}`
    /// rendered readably when it parses, the raw text otherwise.
    Provider { status: u16, body: String },
    /// A 2xx whose body didn't parse as the documented
    /// `{output: {choices: [...]}}` envelope.
    InvalidBody(String),
    /// A 2xx carrying no image at all — refused here rather than handed downstream
    /// as an empty artifact list (see `MediaOutcome::Complete`'s contract).
    NoImages,
    /// Fetching an artifact's URL failed. Pre-rendered from
    /// [`crate::cas::FetchError`], whose own messages name the fix.
    Fetch { index: usize, detail: String },
    /// A fetched artifact's `Content-Type` names something the CAS cannot store as
    /// an image — or was absent entirely. Named rather than guessed: storing bytes
    /// under a wrong extension is the corruption this refuses.
    UnusableContentType { index: usize, got: Option<String> },
}

impl std::fmt::Display for DashScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DashScopeError::Transport(msg) => {
                write!(f, "DashScope request failed: {msg}")
            }
            DashScopeError::Provider { status, body } => {
                write!(f, "DashScope returned {status}: {body}")
            }
            DashScopeError::InvalidBody(msg) => write!(
                f,
                "DashScope 2xx body didn't parse as {{\"output\": {{\"choices\": [...]}}}}: {msg}"
            ),
            DashScopeError::NoImages => write!(
                f,
                "DashScope returned no images — zero artifacts is never treated as a \
                 successful generation"
            ),
            DashScopeError::Fetch { index, detail } => {
                write!(f, "artifact {index}: {detail}")
            }
            DashScopeError::UnusableContentType { index, got } => match got {
                Some(ct) => write!(
                    f,
                    "artifact {index} came back as {ct:?}, which kaibo cannot store as an \
                     image — ask for png, jpeg, webp, or gif"
                ),
                None => write!(
                    f,
                    "artifact {index} came back with no Content-Type, so kaibo cannot tell \
                     what the bytes are — ask for png, jpeg, webp, or gif"
                ),
            },
        }
    }
}

impl std::error::Error for DashScopeError {}

// --- Request -----------------------------------------------------------------

/// Build the JSON body for one generation. Pure: no network, no clock.
///
/// The prompt becomes the single user message's text; caller fields become
/// `parameters`, with [`ENABLE_INTERLEAVE`] seeded true unless the caller named it.
pub fn build_request_body(model: &str, request: &MediaRequest) -> Value {
    let mut parameters = Map::new();
    parameters.insert(ENABLE_INTERLEAVE.to_string(), Value::Bool(true));
    for (name, value) in &request.fields {
        parameters.insert(name.clone(), value.to_json());
    }
    json!({
        "model": model,
        "input": {
            "messages": [{
                "role": "user",
                "content": [{"text": request.prompt}],
            }],
        },
        "parameters": Value::Object(parameters),
    })
}

// --- Response ----------------------------------------------------------------

/// Pull every artifact URL out of a 2xx body, in response order. Pure.
///
/// The envelope is `output.choices[].message.content[].image`; one choice per image.
/// A `content` entry with no `image` (a text part in an interleaved answer) is
/// skipped rather than refused — the images are what this route is asked for.
pub fn parse_response(body: &Value) -> Result<Vec<String>, DashScopeError> {
    let choices = body
        .get("output")
        .and_then(|o| o.get("choices"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| DashScopeError::InvalidBody(truncate_for_error(&body.to_string())))?;
    let mut urls = Vec::new();
    for choice in choices {
        let Some(parts) = choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for part in parts {
            if let Some(url) = part.get("image").and_then(|i| i.as_str()) {
                urls.push(url.to_string());
            }
        }
    }
    if urls.is_empty() {
        return Err(DashScopeError::NoImages);
    }
    Ok(urls)
}

/// The mime for one fetched artifact: the server's own `Content-Type`, accepted only
/// when the CAS can store it as an image.
///
/// Deliberately not a guess. The response JSON carries no per-artifact type, and the
/// URL's extension is a hint an operator cannot correct — so the one authority is
/// what the object store said the bytes are. Anything else is named and refused
/// rather than defaulted to png, because a wrong extension in a content-addressed
/// store is a permanent mislabel under a real digest.
pub fn artifact_mime(index: usize, content_type: Option<&str>) -> Result<String, DashScopeError> {
    let Some(raw) = content_type else {
        return Err(DashScopeError::UnusableContentType { index, got: None });
    };
    let essence = raw.split(';').next().unwrap_or(raw).trim().to_string();
    match crate::cas::Extension::from_mime(&essence) {
        Some(ext) if ext.is_image() => Ok(essence),
        _ => Err(DashScopeError::UnusableContentType {
            index,
            got: Some(essence),
        }),
    }
}

/// Render a provider error body readably: DashScope's `{code, message, request_id}`
/// when it parses that way, the raw text otherwise.
pub fn render_provider_error(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return truncate_for_error(text);
    };
    let code = value.get("code").and_then(|c| c.as_str());
    let message = value.get("message").and_then(|m| m.as_str());
    match (code, message) {
        (Some(c), Some(m)) => format!("{c}: {m}"),
        (None, Some(m)) => m.to_string(),
        _ => truncate_for_error(text),
    }
}

/// Keep a raw body out of the multi-kilobyte range in an error string.
fn truncate_for_error(text: &str) -> String {
    const LIMIT: usize = 1 << 10;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// --- Client ------------------------------------------------------------------

/// A configured DashScope connection: credential, base URL, timeout, and the one
/// HTTPS client built through kaibo's TLS seam.
#[derive(Clone)]
pub struct DashScopeClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    request_timeout: Duration,
}

impl std::fmt::Debug for DashScopeClient {
    /// Manual: never render the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashScopeClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl DashScopeClient {
    /// Build a client. The HTTP client comes from [`crate::tls::https_client`], the
    /// single reqwest build site, so ring is installed and the timeout is bounded.
    pub fn new(api_key: String, base_url: String, request_timeout: Duration) -> AnyResult<Self> {
        Ok(Self {
            http: crate::tls::https_client(request_timeout)?,
            api_key,
            base_url,
            request_timeout,
        })
    }

    /// POST one generation and return the parsed 2xx body.
    async fn post_generation(&self, body: &Value) -> Result<Value, DashScopeError> {
        let response = self
            .http
            .post(generation_url(&self.base_url))
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .json(body)
            .send()
            .await
            .map_err(|e| DashScopeError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| DashScopeError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(DashScopeError::Provider {
                status: status.as_u16(),
                body: render_provider_error(&text),
            });
        }
        serde_json::from_str(&text).map_err(|e| DashScopeError::InvalidBody(e.to_string()))
    }
}

/// One DashScope image model, ready to generate.
pub struct DashScopeImageModel {
    client: DashScopeClient,
    model: String,
}

impl DashScopeImageModel {
    /// Bind a client to one model id — the shape `MediaArm::from_slot` builds.
    pub fn from_parts(client: &DashScopeClient, model: &str) -> Self {
        Self {
            client: client.clone(),
            model: model.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl crate::media::MediaModel for DashScopeImageModel {
    /// Generate, then fetch each artifact's bytes.
    ///
    /// All-or-nothing on purpose, matching `store_generated_artifacts`' posture: one
    /// failed fetch fails the call rather than storing a partial set, so a result is
    /// never quietly short. The images are already generated and billed either way —
    /// the honest outcome is an error naming which artifact failed, not a shorter
    /// list the caller has no way to notice.
    async fn generate(&self, request: &MediaRequest) -> AnyResult<MediaOutcome> {
        let body = build_request_body(&self.model, request);
        let response = self.client.post_generation(&body).await?;
        let urls = parse_response(&response)?;
        let mut artifacts = Vec::with_capacity(urls.len());
        for (index, url) in urls.iter().enumerate() {
            let (bytes, content_type) =
                crate::cas::fetch_artifact_bytes(url, self.client.request_timeout)
                    .await
                    .map_err(|e| DashScopeError::Fetch {
                        index,
                        detail: e.to_string(),
                    })?;
            let mime = artifact_mime(index, content_type.as_deref())?;
            artifacts.push(MediaArtifact {
                bytes,
                mime,
                // DashScope reports no seed, even when the request set one — so
                // there is nothing here to record. A caller's own `seed` field rides
                // the request and is the reproduction handle.
                seed: None,
            });
        }
        Ok(MediaOutcome::Complete(artifacts))
    }

    /// Unreachable: every operation on this route is synchronous, so `generate`
    /// never hands back a job id to poll. Bails loudly rather than returning
    /// `Pending` forever, which would look like a slow provider instead of a bug.
    async fn poll(&self, _job: &MediaJobId) -> AnyResult<MediaPollOutcome> {
        anyhow::bail!(
            "DashScope generations are synchronous, so there is no job to poll — \
             `generate` returns its artifacts in the same call"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::FieldValue;

    fn request(prompt: &str) -> MediaRequest {
        MediaRequest {
            prompt: prompt.to_string(),
            ..Default::default()
        }
    }

    /// The route is appended to the base, and a trailing slash on the operator's
    /// base URL does not double the separator.
    #[test]
    fn generation_url_appends_the_route_to_any_base_spelling() {
        assert_eq!(
            generation_url("https://example.test"),
            "https://example.test/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            generation_url("https://example.test/"),
            generation_url("https://example.test")
        );
    }

    /// A text-only prompt is refused by the provider without `enable_interleave`, so
    /// the body seeds it — the one parameter kaibo supplies unasked.
    #[test]
    fn body_seeds_enable_interleave_for_a_text_only_prompt() {
        let body = build_request_body("wan2.6-t2i", &request("a red cube"));
        assert_eq!(body["parameters"][ENABLE_INTERLEAVE], json!(true));
        assert_eq!(body["model"], json!("wan2.6-t2i"));
        assert_eq!(
            body["input"]["messages"][0]["content"][0]["text"],
            json!("a red cube")
        );
    }

    /// A caller field of the same name replaces the seed rather than colliding with
    /// it — the merge Stability's `output_format` seed already uses.
    #[test]
    fn a_caller_field_replaces_the_seeded_enable_interleave() {
        let mut req = request("a red cube");
        req.fields
            .push((ENABLE_INTERLEAVE.to_string(), FieldValue::Bool(false)));
        let body = build_request_body("wan2.6-t2i", &req);
        assert_eq!(body["parameters"][ENABLE_INTERLEAVE], json!(false));
    }

    /// The caller's JSON type survives to the wire: `n` stays an integer (DashScope
    /// type-checks it), a string stays a string.
    #[test]
    fn caller_field_types_ride_through_unchanged() {
        let mut req = request("a red cube");
        req.fields.push((
            "n".to_string(),
            FieldValue::Num(serde_json::Number::from(2)),
        ));
        req.fields.push((
            "negative_prompt".to_string(),
            FieldValue::Str("blurry".to_string()),
        ));
        let body = build_request_body("wan2.6-t2i", &req);
        assert_eq!(body["parameters"]["n"], json!(2));
        assert!(body["parameters"]["n"].is_i64(), "n must stay an integer");
        assert_eq!(body["parameters"]["negative_prompt"], json!("blurry"));
    }

    /// Every image comes back, in response order — the one-to-many shape, since one
    /// call returns up to four.
    #[test]
    fn parse_collects_every_image_in_order() {
        let body = json!({"output": {"choices": [
            {"message": {"content": [{"image": "https://a.test/1.png", "type": "image"}]}},
            {"message": {"content": [{"image": "https://a.test/2.png", "type": "image"}]}},
        ]}});
        assert_eq!(
            parse_response(&body).unwrap(),
            vec![
                "https://a.test/1.png".to_string(),
                "https://a.test/2.png".to_string()
            ]
        );
    }

    /// A text part in an interleaved answer is skipped; the images are what this
    /// route is asked for.
    #[test]
    fn parse_skips_non_image_content_parts() {
        let body = json!({"output": {"choices": [
            {"message": {"content": [{"text": "here you go"}, {"image": "https://a.test/1.png"}]}},
        ]}});
        assert_eq!(
            parse_response(&body).unwrap(),
            vec!["https://a.test/1.png".to_string()]
        );
    }

    /// A 2xx with no image at all is refused, never handed downstream as an empty
    /// artifact list.
    #[test]
    fn parse_refuses_a_body_with_no_images() {
        let body = json!({"output": {"choices": [
            {"message": {"content": [{"text": "no picture for you"}]}},
        ]}});
        assert_eq!(parse_response(&body), Err(DashScopeError::NoImages));
    }

    /// A body that is not the documented envelope is refused with the body in hand,
    /// not silently treated as zero images.
    #[test]
    fn parse_refuses_an_unrecognized_envelope() {
        let body = json!({"data": [{"url": "https://a.test/1.png"}]});
        let err = parse_response(&body).expect_err("an OpenAI-shaped body is not this envelope");
        assert!(matches!(err, DashScopeError::InvalidBody(_)), "got {err:?}");
    }

    /// The server's Content-Type names the mime, and a charset suffix does not
    /// defeat the match.
    #[test]
    fn mime_comes_from_the_content_type() {
        assert_eq!(artifact_mime(0, Some("image/png")).unwrap(), "image/png");
        assert_eq!(
            artifact_mime(0, Some("image/jpeg; charset=binary")).unwrap(),
            "image/jpeg"
        );
    }

    /// A missing or unstorable Content-Type is named and refused — never defaulted
    /// to png, because a wrong extension under a real digest is a permanent
    /// mislabel.
    #[test]
    fn an_unusable_content_type_is_refused_rather_than_guessed() {
        assert_eq!(
            artifact_mime(1, None),
            Err(DashScopeError::UnusableContentType {
                index: 1,
                got: None
            })
        );
        assert_eq!(
            artifact_mime(2, Some("application/xml")),
            Err(DashScopeError::UnusableContentType {
                index: 2,
                got: Some("application/xml".to_string())
            })
        );
        // text/plain is a mime the CAS knows, but not an image — the image filter is
        // what refuses it, so this would pass a bare `from_mime` check.
        assert!(artifact_mime(3, Some("text/plain")).is_err());
    }

    /// A provider error renders as `code: message`, and an unparseable body keeps
    /// its raw text rather than being swallowed.
    #[test]
    fn provider_errors_render_readably() {
        assert_eq!(
            render_provider_error(
                r#"{"code":"InvalidApiKey","message":"Invalid API-key provided.","request_id":"x"}"#
            ),
            "InvalidApiKey: Invalid API-key provided."
        );
        assert_eq!(
            render_provider_error("<html>502</html>"),
            "<html>502</html>"
        );
    }

    /// Poll is unreachable on a synchronous route and says so, rather than reporting
    /// `Pending` forever.
    #[tokio::test]
    async fn poll_bails_because_every_operation_is_synchronous() {
        use crate::media::MediaModel as _;
        let client = DashScopeClient::new(
            "k".to_string(),
            DASHSCOPE_API_BASE.to_string(),
            Duration::from_secs(30),
        )
        .expect("client builds");
        let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");
        let err = model
            .poll(&MediaJobId("job-1".to_string()))
            .await
            .expect_err("a sync-only kind has nothing to poll");
        assert!(
            format!("{err:#}").contains("synchronous"),
            "the error explains why there is no job: {err:#}"
        );
    }
}
