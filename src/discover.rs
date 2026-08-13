//! Read-only model discovery: ask a backend's provider "what models do you have"
//! instead of hand-rolling a curl + auth header. kaibo already knows every configured
//! backend's base URL, key source, and wire protocol (`config.rs::Backend`), so this
//! module turns that into one GET per backend (paginated where the provider requires
//! it) and a normalized + raw-passthrough result.
//!
//! This is an **operator surface**, not a model-team capability — like `kaibo://config`
//! or `batch list`, it's driven by the MCP tool / CLI subcommand directly, no cast, no
//! model in the loop (see AGENTS.md's "operator surface vs. the model team" invariant).
//! It is also pure network GETs: no filesystem write, no kaish, nothing for
//! `tests/no_write_path.rs` to catch.
//!
//! The shape mirrors `batch.rs`'s provider split: a pure, offline-testable request
//! builder ([`discovery_endpoint`]), pure per-provider parsers ([`parse_openai_models`]/
//! [`parse_anthropic_models`]/[`parse_gemini_models`]), an injectable fetch seam
//! ([`ModelListFetcher`], mirroring the `ScriptedClient` ethos in `test_support.rs`) so
//! the pagination loop is testable with no network, and a real [`HttpModelListFetcher`]
//! built on the one blessed TLS client constructor (`crate::tls::https_client`).

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::batch::rfc3339_to_epoch;
use crate::config::Backend;
use crate::credentials::WireKind;

/// DeepSeek's models-list host. Mirrors rig's private `deepseek` provider constant —
/// same drift-prone class as `config.rs::default_models`: if rig retargets its DeepSeek
/// client, this needs a matching update, and nothing but a live probe will notice.
const DEEPSEEK_BASE: &str = "https://api.deepseek.com";

/// OpenRouter's unified gateway host. Mirrors rig's private `openrouter` provider
/// constant — same drift-prone class as `DEEPSEEK_BASE` above.
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";

/// Anthropic's default API host — used when a backend sets no `base_url` (see
/// `Backend::base_url`'s doc: unset falls back to rig's built-in endpoint).
const ANTHROPIC_DEFAULT_BASE: &str = "https://api.anthropic.com";

/// Gemini's default API host — used when a backend sets no `base_url`.
const GEMINI_DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

/// Hard cap on pagination pages per backend, independent of any cursor-repeat guard —
/// a misbehaving or adversarial endpoint that always claims "more" must not spin
/// forever. 100 pages at Anthropic's `limit=1000` (or a Gemini default page size) is
/// far beyond any real model catalog.
const MAX_PAGES: usize = 100;

/// One model as reported by a provider's list endpoint, normalized across the four
/// wire shapes plus the provider's own object verbatim. Every normalized field is
/// `Option` — a missing or wrong-typed source field is `None`, never a panic (a
/// provider's list endpoint is out of kaibo's control).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredModel {
    pub id: String,
    /// Anthropic `display_name`, Gemini `displayName`.
    pub display_name: Option<String>,
    /// Unix epoch seconds. OpenAI-shaped `created` (already an int) or Anthropic
    /// `created_at` (RFC3339, parsed via [`rfc3339_to_epoch`]) — `None` if absent or
    /// unparseable; the original value still rides in `raw` either way.
    pub created: Option<i64>,
    /// Anthropic `max_input_tokens`, Gemini `inputTokenLimit`.
    pub context_window: Option<u64>,
    /// The model's advertised max *completion* tokens — OpenRouter
    /// `top_provider.max_completion_tokens`, Gemini `outputTokenLimit`. `None` where
    /// the provider's list endpoint doesn't report one (Anthropic, plain OpenAI wire).
    /// Surfaced so a configure-time caller can size a synth slot's `max_tokens` from
    /// it: reasoning bills into the same completion budget as the answer, so a budget
    /// set below the ceiling starves answers.
    pub output_ceiling: Option<u64>,
    /// The provider's per-model object, untouched — the passthrough half of the
    /// contract, so a caller who needs a field we didn't normalize still has it.
    pub raw: Value,
    /// Normalized per-token price, verbatim strings from the provider — `None` when
    /// the endpoint doesn't advertise pricing on `/models` (only `Openai`-wire kinds
    /// do today; see [`parse_openai_models`]).
    pub pricing: Option<ModelPricing>,
    /// Does this model take a reasoning parameter, per the provider's own catalog?
    /// `Some(true)`/`Some(false)` when the endpoint publishes a parameter list,
    /// `None` when it publishes none — an unknown, never a "no".
    ///
    /// Read from `supported_parameters`, which OpenRouter and vLLM-backed gateways
    /// both ship. It answers *whether*, never *which rungs*: no catalog publishes the
    /// ladder, and it moves per model (Crusoe takes `max` on DeepSeek-V4-Flash and
    /// caps at `high` on Nemotron-Omni-Reasoning). A rung a model refuses comes back
    /// as the provider's own error naming what it accepts, which is why kaibo keeps
    /// no allowlist — see [`crate::consult::EffortWire`].
    pub reasoning: Option<bool>,
}

/// Whether a catalog entry says this model takes a reasoning parameter.
///
/// Matched against the spellings the OpenAI-compatible field actually ships:
/// `reasoning_effort` (vLLM and hosted gateways), plus OpenRouter's `reasoning` and
/// `include_reasoning`. A catalog with no `supported_parameters` at all answers `None`
/// — kaibo reports "unknown", never "unsupported", because a missing list is a silent
/// endpoint rather than a negative claim.
fn parse_reasoning_support(item: &Value) -> Option<bool> {
    let params = item.get("supported_parameters")?.as_array()?;
    Some(params.iter().filter_map(Value::as_str).any(|p| {
        matches!(p, "reasoning_effort" | "reasoning" | "include_reasoning")
    }))
}

/// Per-token price, kept as the provider's own string (fractional USD/token) rather
/// than parsed to a float — this is money, and a `f64` round-trip can silently lose
/// precision `.raw` would otherwise preserve exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPricing {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// One page's worth of a GET: the full URL, headers, and query params, fully
/// assertable in a test without a network call. Built by [`discovery_endpoint`] /
/// [`discovery_endpoint_page`]; consumed by [`ModelListFetcher::get_json`].
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryRequest {
    pub url: String,
    /// `(name, value)` pairs, in the order they should be sent.
    pub headers: Vec<(String, String)>,
    /// `(name, value)` query params, in the order they should be sent. Empty means
    /// no query string.
    pub query: Vec<(String, String)>,
}

/// Build the first-page discovery request for `backend`. Pure aside from
/// [`Backend::resolve_key`] (env/key-file read, no network) — mirrors `Arm::from_slot`'s
/// "builds offline" contract (`consult/engine.rs`). Dispatches on `backend.kind`; see
/// the module doc for the per-kind wire shape.
pub fn discovery_endpoint(backend: &Backend) -> Result<DiscoveryRequest> {
    discovery_endpoint_page(backend, None)
}

/// Like [`discovery_endpoint`], but for page N of a paginated listing: `cursor` is the
/// provider's own continuation token (Anthropic's `after_id`, Gemini's `pageToken`).
/// `None` builds the first page. OpenAI-shaped kinds (`Openai`/`OpenRouter`/`DeepSeek`)
/// have no continuation — `cursor` is ignored for them, since [`discover_models`] never
/// loops on those kinds anyway.
pub fn discovery_endpoint_page(backend: &Backend, cursor: Option<&str>) -> Result<DiscoveryRequest> {
    // A media kind publishes no model-listing endpoint in the `/models` shape this
    // module speaks. Refusing loudly beats inventing a URL that would 404, and
    // beats returning an empty list that would read as "this backend serves no
    // models". Classified up front so the wire match below stays media-free; the
    // hint is per media kind, since where a model id comes from differs by kind.
    let Some(wire) = backend.kind.wire() else {
        let hint = match backend.kind.class() {
            crate::credentials::ProviderClass::Media(crate::credentials::MediaKind::Stability) => {
                "the documented generate routes: core, ultra, or an sd3.5 variant"
            }
            crate::credentials::ProviderClass::Media(
                crate::credentials::MediaKind::OpenAiImages,
            ) => "the endpoint's own model names, e.g. gpt-image-1 hosted or whatever a \
                  local sd-server has loaded",
            // The `else` above already proved this kind has no completion wire.
            crate::credentials::ProviderClass::Wire(_) => unreachable!("wire() was None"),
        };
        anyhow::bail!(
            "backend {:?} is kind `{}`, a media kind — it publishes no model-listing \
             endpoint in the `/models` shape this tool speaks. Its models are named on \
             a cast's `image` slot: {hint}",
            backend.name,
            backend.kind.canonical_name()
        )
    };
    let key = backend.resolve_key()?;
    match wire {
        WireKind::Openai => {
            let base = backend.resolved_base_url();
            let base = base.trim_end_matches('/');
            Ok(DiscoveryRequest {
                url: format!("{base}/models"),
                headers: vec![("Authorization".to_string(), format!("Bearer {key}"))],
                query: Vec::new(),
            })
        }
        WireKind::OpenRouter => Ok(DiscoveryRequest {
            url: format!("{OPENROUTER_BASE}/models"),
            headers: vec![("Authorization".to_string(), format!("Bearer {key}"))],
            query: Vec::new(),
        }),
        WireKind::DeepSeek => Ok(DiscoveryRequest {
            url: format!("{DEEPSEEK_BASE}/models"),
            headers: vec![("Authorization".to_string(), format!("Bearer {key}"))],
            query: Vec::new(),
        }),
        WireKind::Anthropic => {
            let base = backend
                .base_url
                .as_deref()
                .unwrap_or(ANTHROPIC_DEFAULT_BASE);
            let base = base.trim_end_matches('/');
            let mut query = vec![("limit".to_string(), "1000".to_string())];
            if let Some(after) = cursor {
                query.push(("after_id".to_string(), after.to_string()));
            }
            Ok(DiscoveryRequest {
                url: format!("{base}/v1/models"),
                headers: vec![
                    ("x-api-key".to_string(), key),
                    (
                        "anthropic-version".to_string(),
                        rig_core::providers::anthropic::completion::ANTHROPIC_VERSION_LATEST
                            .to_string(),
                    ),
                ],
                query,
            })
        }
        WireKind::Gemini => {
            let base = backend.base_url.as_deref().unwrap_or(GEMINI_DEFAULT_BASE);
            let base = base.trim_end_matches('/');
            // No auth header: Gemini authenticates `?key=` as a query param. A
            // key_optional backend's `resolve_key` already resolves to the empty
            // string for this kind (`ProviderKind::placeholder_key`), so this emits a
            // bare `?key=` an ambient-auth gateway accepts — the same keyless-gateway
            // contract the live `consult`/`oneshot` arms rely on (see `#102`).
            let mut query = vec![("key".to_string(), key)];
            if let Some(token) = cursor {
                query.push(("pageToken".to_string(), token.to_string()));
            }
            Ok(DiscoveryRequest {
                url: format!("{base}/v1beta/models"),
                headers: Vec::new(),
                query,
            })
        }
    }
}

/// Parse an OpenAI-shaped `{ data: [{id, created, owned_by, ...}] }` model list —
/// shared by `Openai`, `OpenRouter`, and `DeepSeek` (all three speak the OpenAI wire).
/// Defensive: a missing/wrong-typed field is `None`, never a panic; `.raw` keeps each
/// item verbatim regardless of what we could normalize.
pub fn parse_openai_models(v: &Value) -> Vec<DiscoveredModel> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| DiscoveredModel {
            id: item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            display_name: None,
            created: item.get("created").and_then(Value::as_i64),
            // OpenRouter carries the model's context at the top level, with the
            // serving provider's figure under `top_provider` as the fallback; the
            // other OpenAI-wire endpoints carry neither.
            context_window: item
                .get("context_length")
                .and_then(Value::as_u64)
                .or_else(|| {
                    item.get("top_provider")
                        .and_then(|tp| tp.get("context_length"))
                        .and_then(Value::as_u64)
                }),
            // OpenRouter nests the completion ceiling under `top_provider`; the
            // other OpenAI-wire endpoints simply don't carry the field.
            output_ceiling: item
                .get("top_provider")
                .and_then(|tp| tp.get("max_completion_tokens"))
                .and_then(Value::as_u64),
            pricing: parse_openai_pricing(item),
            reasoning: parse_reasoning_support(item),
            raw: item.clone(),
        })
        .collect()
}

/// Extract a `pricing` object off an OpenAI-shaped model item, if present. Two field
/// conventions in the wild: `input`/`output` (some gateways) and OpenRouter's
/// `prompt`/`completion` — the latter is a fallback, checked only when the former is
/// absent. String-typed only: a price is a fractional-USD-per-token string and we
/// keep it verbatim (never parse to float — precision matters, and `.raw` already
/// carries the original either way); a number-typed price (some providers send one)
/// is left as `None` here rather than reformatted, since round-tripping a JSON number
/// back to a string risks a representation that doesn't match what the provider would
/// consider canonical — `.raw` still has it exactly as sent.
/// `None` unless at least one of input/output is present — an all-empty pricing
/// object would just be noise on every model line.
fn parse_openai_pricing(item: &Value) -> Option<ModelPricing> {
    let pricing = item.get("pricing")?;
    let price = |primary: &str, fallback: &str| -> Option<String> {
        pricing
            .get(primary)
            .or_else(|| pricing.get(fallback))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let input = price("input", "prompt");
    let output = price("output", "completion");
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(ModelPricing { input, output })
}

/// Parse an Anthropic `{ data: [{id, display_name, created_at, max_input_tokens, ...}],
/// has_more, last_id }` page into its models. Pagination fields (`has_more`/`last_id`)
/// are read separately by [`discover_models`]'s loop — this is just the per-model
/// normalization, so it's directly testable against one canned page.
pub fn parse_anthropic_models(v: &Value) -> Vec<DiscoveredModel> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| DiscoveredModel {
            id: item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            display_name: item
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            created: item
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(rfc3339_to_epoch),
            context_window: item.get("max_input_tokens").and_then(Value::as_u64),
            // Anthropic's `/v1/models` advertises neither pricing nor an output ceiling.
            output_ceiling: None,
            pricing: None,
            reasoning: None,
            raw: item.clone(),
        })
        .collect()
}

/// Parse a Gemini `{ models: [{name: "models/x", displayName, inputTokenLimit, ...}],
/// nextPageToken }` page into its models. `name` is `models/{id}` — we strip the
/// `models/` prefix into `id`, but `.raw` keeps the original `name` field untouched.
pub fn parse_gemini_models(v: &Value) -> Vec<DiscoveredModel> {
    v.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            DiscoveredModel {
                id: name.strip_prefix("models/").unwrap_or(name).to_string(),
                display_name: item
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                created: None,
                context_window: item.get("inputTokenLimit").and_then(Value::as_u64),
                output_ceiling: item.get("outputTokenLimit").and_then(Value::as_u64),
                // Gemini's `/v1beta/models` doesn't advertise pricing.
                pricing: None,
                reasoning: None,
                raw: item.clone(),
            }
        })
        .collect()
}

/// The fetch seam [`discover_models`] drives — injectable so the pagination loop is
/// offline-testable, mirroring the `ScriptedClient` ethos in `test_support.rs` (a
/// content-driven responder, not a queue-pop mock, though here the request shape is
/// simple enough that a page-indexed script is honest: each page is requested exactly
/// once, in order, with no concurrent fan-out).
#[async_trait]
pub trait ModelListFetcher: Send + Sync {
    async fn get_json(&self, req: &DiscoveryRequest) -> Result<Value>;
}

/// The real [`ModelListFetcher`]: one GET through the blessed TLS client constructor
/// (`crate::tls::https_client`) — the **only** reqwest-client construction site this
/// module uses, so the rustls-ring/no-aws-lc contract stays in the one place it's
/// supposed to live.
pub struct HttpModelListFetcher {
    request_timeout: Duration,
}

impl HttpModelListFetcher {
    /// `request_timeout` should come from the backend being queried
    /// (`Backend::request_timeout`), so a slow local server gets the same patience a
    /// completion call would.
    pub fn new(request_timeout: Duration) -> Self {
        Self { request_timeout }
    }
}

#[async_trait]
impl ModelListFetcher for HttpModelListFetcher {
    async fn get_json(&self, req: &DiscoveryRequest) -> Result<Value> {
        let client = crate::tls::https_client(self.request_timeout)?;
        let mut builder = client.get(&req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if !req.query.is_empty() {
            builder = builder.query(&req.query);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| anyhow!("model list request to {}: {e}", req.url))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading model list response body from {}: {e}", req.url))?;
        if !status.is_success() {
            return Err(anyhow!("model list {status} from {}: {text}", req.url));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("parsing model list JSON from {}: {text}", req.url))
    }
}

/// Fetch every model `backend` advertises, paginating where the provider requires it:
/// Anthropic loops on `has_more`/`last_id` (`after_id` cursor, `limit=1000`), Gemini
/// loops on `nextPageToken` (`pageToken` query); `Openai`/`OpenRouter`/`DeepSeek` are
/// single-page. Guarded against a misbehaving provider two ways: a hard [`MAX_PAGES`]
/// cap, and an early stop if a cursor repeats (a provider that echoes back the same
/// "next" token would otherwise spin forever even under the page cap).
pub async fn discover_models(
    backend: &Backend,
    fetcher: &impl ModelListFetcher,
) -> Result<Vec<DiscoveredModel>> {
    // A media kind has no listing endpoint — `discovery_endpoint_page` carries the
    // loud refusal, so `list_models` over a mixed backend set names the one it
    // cannot enumerate instead of quietly omitting it.
    let Some(wire) = backend.kind.wire() else {
        return discovery_endpoint_page(backend, None).map(|_| Vec::new());
    };
    match wire {
        WireKind::Anthropic => discover_paginated(
            backend,
            fetcher,
            parse_anthropic_models,
            |body| body.get("has_more").and_then(Value::as_bool).unwrap_or(false),
            |body| {
                body.get("last_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            },
        )
        .await,
        WireKind::Gemini => discover_paginated(
            backend,
            fetcher,
            parse_gemini_models,
            |body| body.get("nextPageToken").and_then(Value::as_str).is_some(),
            |body| {
                body.get("nextPageToken")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            },
        )
        .await,
        WireKind::Openai | WireKind::OpenRouter | WireKind::DeepSeek => {
            let req = discovery_endpoint(backend)?;
            let body = fetcher.get_json(&req).await?;
            Ok(parse_openai_models(&body))
        }
    }
}

/// The shared pagination loop both cursor-based kinds (Anthropic, Gemini) drive, with
/// their differing "is there another page" / "what's the cursor" reads injected —
/// pulling the loop out once so the [`MAX_PAGES`] cap and the cursor-repeat guard live
/// in exactly one place instead of being duplicated per kind.
async fn discover_paginated(
    backend: &Backend,
    fetcher: &impl ModelListFetcher,
    parse: fn(&Value) -> Vec<DiscoveredModel>,
    has_more: fn(&Value) -> bool,
    next_cursor: fn(&Value) -> Option<String>,
) -> Result<Vec<DiscoveredModel>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    for _ in 0..MAX_PAGES {
        let req = discovery_endpoint_page(backend, cursor.as_deref())?;
        let body = fetcher.get_json(&req).await?;
        all.extend(parse(&body));
        if !has_more(&body) {
            break;
        }
        let Some(next) = next_cursor(&body) else {
            break;
        };
        if !seen_cursors.insert(next.clone()) {
            // The provider handed back a cursor we've already used — stop rather than
            // spin, even though we haven't hit MAX_PAGES yet.
            break;
        }
        cursor = Some(next);
    }
    Ok(all)
}

/// Render a multi-backend sweep as prose: one section per backend, each model's `id` +
/// `display_name` (when present) + `context_window`/`output_ceiling` (when present); a backend that
/// errored gets an inline error line instead of a model list. Shared by the MCP tool
/// and the CLI so the two faces can't drift on what a sweep looks like (the
/// `render_config_resource` discipline).
pub fn render_models(results: &BTreeMap<String, Result<Vec<DiscoveredModel>, String>>) -> String {
    if results.is_empty() {
        return "(no backend configured)".to_string();
    }
    let mut out = String::new();
    for (name, result) in results {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("## {name}\n"));
        match result {
            Ok(models) if models.is_empty() => {
                out.push_str("(no models reported)\n");
            }
            Ok(models) => {
                for m in models {
                    let mut line = format!("- `{}`", m.id);
                    if let Some(dn) = &m.display_name {
                        line.push_str(&format!(" — {dn}"));
                    }
                    // Context window and output ceiling share one paren group; either
                    // may be absent alone, and an absent fact renders nothing (no
                    // placeholder noise).
                    let mut limits = Vec::new();
                    if let Some(ctx) = m.context_window {
                        limits.push(format!("context: {ctx}"));
                    }
                    if let Some(ceiling) = m.output_ceiling {
                        limits.push(format!("output: {ceiling}"));
                    }
                    if !limits.is_empty() {
                        line.push_str(&format!(" ({})", limits.join(", ")));
                    }
                    if m.reasoning == Some(true) {
                        line.push_str(" [reasoning]");
                    }
                    if let Some(p) = &m.pricing {
                        let mut parts = Vec::new();
                        if let Some(input) = &p.input {
                            parts.push(format!("in ${input}/tok"));
                        }
                        if let Some(output) = &p.output {
                            parts.push(format!("out ${output}/tok"));
                        }
                        if !parts.is_empty() {
                            line.push_str(&format!(" [{}]", parts.join(", ")));
                        }
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            Err(e) => {
                out.push_str(&format!("error: {e}\n"));
            }
        }
    }
    out
}

/// The same multi-backend sweep as a structured JSON envelope — the `--json` /
/// `structured_content` face of [`render_models`]. Per backend: `ok` + either `models`
/// (each carrying the normalized fields plus `raw`) or `error`.
pub fn models_json_envelope(results: &BTreeMap<String, Result<Vec<DiscoveredModel>, String>>) -> Value {
    serde_json::json!({
        "backends": results
            .iter()
            .map(|(name, result)| match result {
                Ok(models) => serde_json::json!({
                    "backend": name,
                    "ok": true,
                    "models": models
                        .iter()
                        .map(|m| serde_json::json!({
                            "id": m.id,
                            "display_name": m.display_name,
                            "created": m.created,
                            "context_window": m.context_window,
                            "output_ceiling": m.output_ceiling,
                            "pricing": m.pricing.as_ref().map(|p| serde_json::json!({
                                "input": p.input,
                                "output": p.output,
                            })),
                            "raw": m.raw,
                        }))
                        .collect::<Vec<_>>(),
                }),
                Err(e) => serde_json::json!({
                    "backend": name,
                    "ok": false,
                    "error": e,
                }),
            })
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DataCollection, OpenaiWire};
    use crate::credentials::ProviderKind;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn backend(kind: ProviderKind) -> Backend {
        Backend {
            name: "test".to_string(),
            kind,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: DataCollection::default(),
            wire: None,
        }
    }

    /// A tempdir-backed key file, so tests set a backend's credential without touching
    /// real process environment (`env::set_var`/`remove_var` are unsafe and shared
    /// process-wide state — a key file is the isolated, no-`unsafe` way the rest of the
    /// suite already resolves a key; see `tests/config.rs::local_backend`). The
    /// `TempDir` must outlive the `Backend` using its path, so callers keep the guard
    /// alive alongside the backend.
    fn key_file(contents: &str) -> (TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir for a test key file");
        let path = dir.path().join("key");
        std::fs::write(&path, contents).expect("write test key file");
        (dir, path.to_string_lossy().to_string())
    }

    // --- discovery_endpoint: exact URL/headers/query per kind -------------------

    #[test]
    fn openai_endpoint_is_bearer_at_base_slash_models() {
        let mut b = backend(ProviderKind::Openai);
        b.base_url = Some("http://localhost:9999/v1".to_string());
        b.key_optional = true;
        let req = discovery_endpoint(&b).expect("openai endpoint builds offline");
        assert_eq!(req.url, "http://localhost:9999/v1/models");
        assert_eq!(
            req.headers,
            vec![(
                "Authorization".to_string(),
                format!("Bearer {}", crate::credentials::PLACEHOLDER_OPENAI_KEY)
            )]
        );
        assert!(req.query.is_empty());
    }

    #[test]
    fn openai_endpoint_sends_bearer_even_for_the_keyless_placeholder() {
        // Spec: for OpenAI-wire kinds, send the bearer even when it's the keyless
        // placeholder — harmless to a local server, mirrors how rig auths.
        let mut b = backend(ProviderKind::Openai);
        b.base_url = Some("http://localhost:9999/v1".to_string());
        b.key_optional = true;
        let req = discovery_endpoint(&b).unwrap();
        let (name, value) = &req.headers[0];
        assert_eq!(name, "Authorization");
        assert!(value.contains(crate::credentials::PLACEHOLDER_OPENAI_KEY));
    }

    #[test]
    fn openrouter_endpoint_is_fixed_base_bearer() {
        let (_dir, path) = key_file("or-key");
        let mut b = backend(ProviderKind::OpenRouter);
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, format!("{OPENROUTER_BASE}/models"));
        assert_eq!(
            req.headers,
            vec![("Authorization".to_string(), "Bearer or-key".to_string())]
        );
        assert!(req.query.is_empty());
    }

    #[test]
    fn deepseek_endpoint_is_fixed_base_bearer() {
        let (_dir, path) = key_file("ds-key");
        let mut b = backend(ProviderKind::DeepSeek);
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, format!("{DEEPSEEK_BASE}/models"));
        assert_eq!(
            req.headers,
            vec![("Authorization".to_string(), "Bearer ds-key".to_string())]
        );
    }

    #[test]
    fn anthropic_endpoint_defaults_base_and_carries_version_header() {
        let (_dir, path) = key_file("anthropic-key");
        let mut b = backend(ProviderKind::Anthropic);
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, format!("{ANTHROPIC_DEFAULT_BASE}/v1/models"));
        assert!(req
            .headers
            .contains(&("x-api-key".to_string(), "anthropic-key".to_string())));
        assert!(req.headers.contains(&(
            "anthropic-version".to_string(),
            rig_core::providers::anthropic::completion::ANTHROPIC_VERSION_LATEST.to_string()
        )));
        assert_eq!(
            req.query,
            vec![("limit".to_string(), "1000".to_string())]
        );
    }

    #[test]
    fn anthropic_endpoint_honors_a_base_url_gateway_override() {
        let (_dir, path) = key_file("gw-key");
        let mut b = backend(ProviderKind::Anthropic);
        b.base_url = Some("https://gateway.internal/".to_string());
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, "https://gateway.internal/v1/models");
    }

    #[test]
    fn anthropic_page_carries_after_id_cursor() {
        let (_dir, path) = key_file("k");
        let mut b = backend(ProviderKind::Anthropic);
        b.api_key_file = Some(path);
        let req = discovery_endpoint_page(&b, Some("model_123")).unwrap();
        assert_eq!(
            req.query,
            vec![
                ("limit".to_string(), "1000".to_string()),
                ("after_id".to_string(), "model_123".to_string())
            ]
        );
    }

    #[test]
    fn gemini_endpoint_uses_query_key_no_header() {
        let (_dir, path) = key_file("gem-key");
        let mut b = backend(ProviderKind::Gemini);
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, format!("{GEMINI_DEFAULT_BASE}/v1beta/models"));
        assert!(req.headers.is_empty(), "gemini must send no auth header");
        assert_eq!(req.query, vec![("key".to_string(), "gem-key".to_string())]);
    }

    #[test]
    fn gemini_endpoint_honors_a_base_url_gateway_override() {
        let (_dir, path) = key_file("gw-key");
        let mut b = backend(ProviderKind::Gemini);
        b.base_url = Some("https://gateway.internal".to_string());
        b.api_key_file = Some(path);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, "https://gateway.internal/v1beta/models");
    }

    #[test]
    fn gemini_keyless_backend_sends_an_empty_query_key() {
        // Mirrors the #102 fix: a key_optional Gemini backend resolves to the EMPTY
        // string, not the non-empty bearer placeholder, so kaibo emits a bare `?key=`
        // an ambient-auth gateway accepts.
        let mut b = backend(ProviderKind::Gemini);
        b.key_optional = true;
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.query, vec![("key".to_string(), String::new())]);
    }

    #[test]
    fn gemini_page_carries_page_token() {
        let mut b = backend(ProviderKind::Gemini);
        b.key_optional = true;
        let req = discovery_endpoint_page(&b, Some("tok-1")).unwrap();
        assert_eq!(
            req.query,
            vec![
                ("key".to_string(), String::new()),
                ("pageToken".to_string(), "tok-1".to_string())
            ]
        );
    }

    #[test]
    fn missing_key_on_a_keyed_backend_errors_loudly() {
        // A keyed backend with no key configured must error (loud over silent),
        // exactly like `Arm::from_slot`/`resolve_key` for a live completion.
        let b = backend(ProviderKind::Anthropic);
        assert!(discovery_endpoint(&b).is_err());
    }

    #[test]
    fn openai_wire_field_does_not_affect_discovery() {
        // The `wire` (responses vs chat) knob shapes interactive completions only;
        // discovery always hits the plain REST models endpoint regardless.
        let mut b = backend(ProviderKind::Openai);
        b.base_url = Some("http://localhost:9999/v1".to_string());
        b.key_optional = true;
        b.wire = Some(OpenaiWire::Responses);
        let req = discovery_endpoint(&b).unwrap();
        assert_eq!(req.url, "http://localhost:9999/v1/models");
    }

    // --- parse_*: canned provider bodies, normalized fields + untouched .raw ---

    #[test]
    fn parse_openai_shape() {
        let body = serde_json::json!({
            "data": [
                {"id": "gpt-5", "object": "model", "created": 1_700_000_000, "owned_by": "openai"},
                {"id": "gpt-4", "object": "model", "created": 1_600_000_000, "owned_by": "openai"},
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-5");
        assert_eq!(models[0].created, Some(1_700_000_000));
        assert_eq!(models[0].display_name, None);
        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].raw, body["data"][0]);
    }

    #[test]
    fn parse_openai_shape_normalizes_openrouter_output_ceiling() {
        // OpenRouter's catalog nests the completion ceiling under `top_provider` —
        // the field a configure-time caller sizes a synth `max_tokens` from.
        let body = serde_json::json!({
            "data": [
                {
                    "id": "moonshotai/kimi-k2.7-code",
                    "context_length": 262_144,
                    "top_provider": {
                        "context_length": 131_072,
                        "max_completion_tokens": 16_384,
                    },
                },
                {"id": "no-ceiling/model", "top_provider": {"context_length": 8192}},
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(models[0].output_ceiling, Some(16_384));
        assert_eq!(models[1].output_ceiling, None);
        // Context rides the same catalog: the model-level `context_length` wins,
        // the `top_provider` figure is the fallback (live catalogs carry both).
        assert_eq!(models[0].context_window, Some(262_144));
        assert_eq!(models[1].context_window, Some(8192));
        // .raw keeps the nested object untouched either way.
        assert_eq!(
            models[0].raw["top_provider"]["max_completion_tokens"],
            16_384
        );
    }

    #[test]
    fn parse_openai_shape_is_defensive_on_missing_fields() {
        let body = serde_json::json!({ "data": [ {"object": "model"} ] });
        let models = parse_openai_models(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "");
        assert_eq!(models[0].created, None);
    }

    #[test]
    fn parse_openai_shape_handles_a_missing_data_array() {
        let body = serde_json::json!({ "unexpected": "shape" });
        assert!(parse_openai_models(&body).is_empty());
    }

    #[test]
    fn parse_anthropic_shape() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "claude-x",
                    "display_name": "Claude X",
                    "created_at": "2026-01-01T00:00:00Z",
                    "max_input_tokens": 200_000,
                    "capabilities": ["vision"],
                }
            ],
            "has_more": true,
            "last_id": "claude-x",
        });
        let models = parse_anthropic_models(&body);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "claude-x");
        assert_eq!(m.display_name.as_deref(), Some("Claude X"));
        assert_eq!(m.created, Some(1_767_225_600));
        assert_eq!(m.context_window, Some(200_000));
        assert_eq!(m.output_ceiling, None, "anthropic's list reports no ceiling");
        assert_eq!(m.raw, body["data"][0]);
    }

    #[test]
    fn parse_anthropic_shape_leaves_created_none_on_an_unparseable_timestamp() {
        let body = serde_json::json!({
            "data": [ {"id": "x", "created_at": "not-a-timestamp"} ]
        });
        let models = parse_anthropic_models(&body);
        assert_eq!(models[0].created, None);
        // The original string still rides in .raw.
        assert_eq!(models[0].raw["created_at"], "not-a-timestamp");
    }

    #[test]
    fn parse_gemini_shape_strips_the_models_prefix() {
        let body = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-x",
                    "displayName": "Gemini X",
                    "inputTokenLimit": 1_000_000,
                    "outputTokenLimit": 8192,
                }
            ],
            "nextPageToken": "tok-2",
        });
        let models = parse_gemini_models(&body);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "gemini-x");
        assert_eq!(m.display_name.as_deref(), Some("Gemini X"));
        assert_eq!(m.context_window, Some(1_000_000));
        assert_eq!(m.output_ceiling, Some(8192));
        assert_eq!(m.created, None);
        // .raw keeps the original "models/..." name untouched.
        assert_eq!(m.raw["name"], "models/gemini-x");
    }

    #[test]
    fn parse_gemini_shape_handles_a_missing_models_array() {
        let body = serde_json::json!({ "nextPageToken": null });
        assert!(parse_gemini_models(&body).is_empty());
    }

    // --- pagination: a scripted ModelListFetcher, no network -------------------

    /// A scripted fetcher returning one canned body per call, in order — content-driven
    /// enough for this seam (one request in flight at a time, no concurrent fan-out),
    /// unlike the real consult loop's `ScriptedClient`.
    struct ScriptedFetcher {
        pages: Mutex<Vec<Value>>,
        calls: Mutex<Vec<DiscoveryRequest>>,
    }

    impl ScriptedFetcher {
        fn new(pages: Vec<Value>) -> Self {
            Self {
                pages: Mutex::new(pages),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelListFetcher for ScriptedFetcher {
        async fn get_json(&self, req: &DiscoveryRequest) -> Result<Value> {
            self.calls.lock().unwrap().push(req.clone());
            let mut pages = self.pages.lock().unwrap();
            if pages.is_empty() {
                return Err(anyhow!("scripted fetcher ran out of pages"));
            }
            Ok(pages.remove(0))
        }
    }

    #[tokio::test]
    async fn anthropic_pagination_follows_has_more_and_last_id() {
        let page1 = serde_json::json!({
            "data": [{"id": "a"}],
            "has_more": true,
            "last_id": "a",
        });
        let page2 = serde_json::json!({
            "data": [{"id": "b"}],
            "has_more": false,
            "last_id": "b",
        });
        let fetcher = ScriptedFetcher::new(vec![page1, page2]);
        let mut b = backend(ProviderKind::Anthropic);
        b.key_optional = true;
        // Anthropic doesn't tolerate a missing key in practice, but the scripted
        // fetcher never hits a real network — key_optional here just lets
        // discovery_endpoint build offline without configuring env/file credentials.
        let models = discover_models(&b, &fetcher).await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);

        let calls = fetcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].query.iter().any(|(k, _)| k == "after_id"));
        assert!(calls[1]
            .query
            .contains(&("after_id".to_string(), "a".to_string())));
    }

    #[tokio::test]
    async fn gemini_pagination_follows_next_page_token() {
        let page1 = serde_json::json!({
            "models": [{"name": "models/a"}],
            "nextPageToken": "tok-1",
        });
        let page2 = serde_json::json!({
            "models": [{"name": "models/b"}],
        });
        let fetcher = ScriptedFetcher::new(vec![page1, page2]);
        let mut b = backend(ProviderKind::Gemini);
        b.key_optional = true;
        let models = discover_models(&b, &fetcher).await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);

        let calls = fetcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1]
            .query
            .contains(&("pageToken".to_string(), "tok-1".to_string())));
    }

    #[tokio::test]
    async fn openai_shaped_kinds_never_paginate() {
        let page1 = serde_json::json!({ "data": [{"id": "only-page"}] });
        let fetcher = ScriptedFetcher::new(vec![page1]);
        let mut b = backend(ProviderKind::Openai);
        b.base_url = Some("http://localhost:9999/v1".to_string());
        b.key_optional = true;
        let models = discover_models(&b, &fetcher).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(fetcher.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_repeating_cursor_stops_the_loop_instead_of_spinning() {
        // A misbehaving provider that echoes the same "next" cursor forever must not
        // hang discover_models — the repeat guard breaks the loop even though
        // has_more never says stop.
        let page = serde_json::json!({
            "data": [{"id": "x"}],
            "has_more": true,
            "last_id": "same-cursor",
        });
        // Enough identical pages to prove we stop well short of MAX_PAGES.
        let fetcher = ScriptedFetcher::new(vec![page.clone(), page.clone(), page]);
        let mut b = backend(ProviderKind::Anthropic);
        b.key_optional = true;
        let models = discover_models(&b, &fetcher).await.unwrap();
        // First page has no cursor set yet (cursor=None), so it's fetched; its
        // last_id becomes the cursor for page 2, which repeats it — loop stops there.
        assert_eq!(models.len(), 2, "expected exactly two pages before the repeat guard stops the loop");
        assert_eq!(fetcher.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fetch_error_propagates_out_of_discover_models() {
        struct FailingFetcher;
        #[async_trait]
        impl ModelListFetcher for FailingFetcher {
            async fn get_json(&self, _req: &DiscoveryRequest) -> Result<Value> {
                Err(anyhow!("boom"))
            }
        }
        let mut b = backend(ProviderKind::Openai);
        b.base_url = Some("http://localhost:9999/v1".to_string());
        b.key_optional = true;
        let err = discover_models(&b, &FailingFetcher).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    // --- render_models / models_json_envelope -----------------------------------

    #[test]
    fn render_models_mixed_success_and_error() {
        let mut results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        results.insert(
            "ok-backend".to_string(),
            Ok(vec![DiscoveredModel {
                id: "m1".to_string(),
                display_name: Some("Model One".to_string()),
                created: None,
                context_window: Some(128_000),
                output_ceiling: Some(16_384),
                pricing: Some(ModelPricing {
                    input: Some("0.0000015".to_string()),
                    output: Some("0.000006".to_string()),
                }),
                reasoning: None,
                raw: serde_json::json!({"id": "m1"}),
            }]),
        );
        results.insert("broken-backend".to_string(), Err("no key".to_string()));

        let text = render_models(&results);
        assert!(text.contains("## broken-backend"));
        assert!(text.contains("error: no key"));
        assert!(text.contains("## ok-backend"));
        assert!(text.contains("`m1`"));
        assert!(text.contains("Model One"));
        assert!(text.contains("128000") || text.contains("128,000"));
        assert!(
            text.contains("(context: 128000, output: 16384)"),
            "expected the output ceiling beside context, got: {text}"
        );
        assert!(
            text.contains("[in $0.0000015/tok, out $0.000006/tok]"),
            "expected a pricing suffix, got: {text}"
        );
    }

    #[test]
    fn render_models_shows_a_ceiling_without_a_context_window() {
        // The OpenRouter shape today: ceiling normalized, context not — the paren
        // group must render the one fact it has, with no placeholder for the other.
        let mut results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        results.insert(
            "or".to_string(),
            Ok(vec![DiscoveredModel {
                id: "m1".to_string(),
                display_name: None,
                created: None,
                context_window: None,
                output_ceiling: Some(16_384),
                pricing: None,
                reasoning: None,
                raw: serde_json::json!({"id": "m1"}),
            }]),
        );
        let text = render_models(&results);
        assert!(text.contains("(output: 16384)"), "got: {text}");
        assert!(!text.contains("context"), "no context means no context label, got: {text}");
    }

    #[test]
    fn render_models_omits_pricing_suffix_when_absent() {
        let mut results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        results.insert(
            "ok-backend".to_string(),
            Ok(vec![DiscoveredModel {
                id: "m1".to_string(),
                display_name: None,
                created: None,
                context_window: None,
                output_ceiling: None,
                pricing: None,
                reasoning: None,
                raw: serde_json::json!({"id": "m1"}),
            }]),
        );
        let text = render_models(&results);
        assert!(!text.contains('['), "no pricing means no bracketed suffix, got: {text}");
    }

    #[test]
    fn render_models_empty_sweep() {
        let results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        assert_eq!(render_models(&results), "(no backend configured)");
    }

    #[test]
    fn json_envelope_shape_mixed_success_and_error() {
        let mut results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        results.insert(
            "ok-backend".to_string(),
            Ok(vec![DiscoveredModel {
                id: "m1".to_string(),
                display_name: None,
                created: Some(42),
                context_window: None,
                output_ceiling: Some(65_536),
                pricing: Some(ModelPricing {
                    input: Some("0.000001".to_string()),
                    output: None,
                }),
                reasoning: None,
                raw: serde_json::json!({"id": "m1", "extra": "field"}),
            }]),
        );
        results.insert("broken-backend".to_string(), Err("no key".to_string()));

        let env = models_json_envelope(&results);
        let backends = env["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 2);
        let ok = backends
            .iter()
            .find(|b| b["backend"] == "ok-backend")
            .unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["models"][0]["id"], "m1");
        assert_eq!(ok["models"][0]["created"], 42);
        assert_eq!(ok["models"][0]["raw"]["extra"], "field");
        assert_eq!(ok["models"][0]["output_ceiling"], 65_536);
        assert_eq!(ok["models"][0]["pricing"]["input"], "0.000001");
        assert!(ok["models"][0]["pricing"]["output"].is_null());

        let broken = backends
            .iter()
            .find(|b| b["backend"] == "broken-backend")
            .unwrap();
        assert_eq!(broken["ok"], false);
        assert_eq!(broken["error"], "no key");
    }

    #[test]
    fn json_envelope_pricing_is_null_not_an_empty_object_when_absent() {
        let mut results: BTreeMap<String, Result<Vec<DiscoveredModel>, String>> = BTreeMap::new();
        results.insert(
            "ok-backend".to_string(),
            Ok(vec![DiscoveredModel {
                id: "m1".to_string(),
                display_name: None,
                created: None,
                context_window: None,
                output_ceiling: None,
                pricing: None,
                reasoning: None,
                raw: serde_json::json!({"id": "m1"}),
            }]),
        );
        let env = models_json_envelope(&results);
        assert!(env["backends"][0]["models"][0]["pricing"].is_null());
    }

    // --- parse_openai_pricing: both field-name conventions, no unwrap on --------
    // wrong-typed / missing input.

    #[test]
    fn parse_openai_shape_normalizes_input_output_pricing() {
        let body = serde_json::json!({
            "data": [
                {"id": "m1", "pricing": {"input": "0.0000015", "output": "0.000006"}}
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(
            models[0].pricing,
            Some(ModelPricing {
                input: Some("0.0000015".to_string()),
                output: Some("0.000006".to_string()),
            })
        );
        // .raw keeps the original pricing object untouched.
        assert_eq!(
            models[0].raw["pricing"],
            serde_json::json!({"input": "0.0000015", "output": "0.000006"})
        );
    }

    #[test]
    fn parse_openai_shape_falls_back_to_openrouter_prompt_completion_pricing() {
        let body = serde_json::json!({
            "data": [
                {"id": "m1", "pricing": {"prompt": "0.000001", "completion": "0.000002"}}
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(
            models[0].pricing,
            Some(ModelPricing {
                input: Some("0.000001".to_string()),
                output: Some("0.000002".to_string()),
            })
        );
        assert_eq!(
            models[0].raw["pricing"],
            serde_json::json!({"prompt": "0.000001", "completion": "0.000002"})
        );
    }

    #[test]
    fn parse_openai_shape_pricing_is_none_when_the_endpoint_reports_none() {
        let body = serde_json::json!({ "data": [ {"id": "m1"} ] });
        let models = parse_openai_models(&body);
        assert_eq!(models[0].pricing, None);
        assert!(models[0].raw.get("pricing").is_none());
    }

    #[test]
    fn parse_openai_shape_pricing_is_none_not_some_when_the_object_is_all_empty() {
        // An all-empty pricing object (neither convention present) is noise, not
        // a Some(ModelPricing { None, None }).
        let body = serde_json::json!({
            "data": [ {"id": "m1", "pricing": {}} ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(models[0].pricing, None);
        // .raw still carries the (empty) pricing object verbatim.
        assert_eq!(models[0].raw["pricing"], serde_json::json!({}));
    }

}

#[cfg(test)]
mod reasoning_support_tests {
    use super::*;

    /// The catalog answers *whether* a model reasons, never *which rungs* it takes.
    ///
    /// A missing `supported_parameters` is `None` — unknown, not "no". The distinction
    /// matters because most endpoints publish no parameter list at all, and rendering
    /// those as unsupported would tell an operator the opposite of the truth.
    #[test]
    fn reasoning_support_reads_the_catalog_and_admits_when_it_cannot() {
        let takes = serde_json::json!({
            "id": "m",
            "supported_parameters": ["temperature", "reasoning_effort", "top_p"]
        });
        assert_eq!(parse_reasoning_support(&takes), Some(true));

        // OpenRouter's spellings, not vLLM's.
        let openrouter = serde_json::json!({ "id": "m", "supported_parameters": ["reasoning"] });
        assert_eq!(parse_reasoning_support(&openrouter), Some(true));

        let listed_without = serde_json::json!({
            "id": "m",
            "supported_parameters": ["temperature", "top_p"]
        });
        assert_eq!(
            parse_reasoning_support(&listed_without),
            Some(false),
            "a published list that omits it IS a negative claim"
        );

        let silent = serde_json::json!({ "id": "m" });
        assert_eq!(
            parse_reasoning_support(&silent),
            None,
            "no list at all is UNKNOWN — reporting `false` here would state the \
             opposite of the truth for every endpoint that publishes nothing"
        );
    }

    /// Only a positive answer earns a marker: an unknown must not read as a "no", and
    /// a line cluttered with `[reasoning: unknown]` on every plain endpoint is noise.
    #[test]
    fn only_a_known_reasoning_model_is_marked() {
        let mk = |reasoning| DiscoveredModel {
            id: "m".into(),
            display_name: None,
            created: None,
            context_window: None,
            output_ceiling: None,
            pricing: None,
            reasoning,
            raw: serde_json::json!({}),
        };
        let render = |r| {
            let mut results = BTreeMap::new();
            results.insert("b".to_string(), Ok(vec![mk(r)]));
            render_models(&results)
        };
        assert!(render(Some(true)).contains("[reasoning]"));
        assert!(!render(Some(false)).contains("[reasoning]"));
        assert!(
            !render(None).contains("[reasoning]"),
            "unknown must not render as either answer"
        );
    }
}
