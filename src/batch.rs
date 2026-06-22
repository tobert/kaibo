//! Batch — offline, max-effort fan-out of the tool-less answer.
//!
//! kaibo's consultation tools ([`crate::consult`]) run *synchronously*: prompt in,
//! answer out, the call held open until the model replies. Batch is the **offline,
//! async sibling** of [`oneshot`](crate::consult::oneshot) — submit a list of
//! questions, get a handle, poll it, read the answers when they land. The caller
//! never holds a synchronous call open per answer; the provider runs the work on its
//! own (cheaper) batch lane and kaibo just hands back the id.
//!
//! **Toolless by construction — no agents.** A batch item is a question, not a
//! `run_phase` loop: no kaish, no explorer, no tool loop. That's forced, not just
//! convenient — provider batch APIs are offline/async and can't drive an interactive
//! tool loop. So batch is built on the `oneshot` *shape* (a single capable model
//! answering from what it was handed), never `consult`.
//!
//! **Max the knobs by default.** Batch is the cheap/async lane, so it spends: it
//! floors `max_tokens` at [`BATCH_MAX_TOKENS_FLOOR`] and forces thinking on at
//! [`BATCH_EFFORT`] regardless of how the cast's synth slot was tuned for interactive
//! use. The latency that makes max-thinking painful synchronously is free here — the
//! caller already accepted "come back later." This is the lane for asking the best
//! model the hard question and waiting.
//!
//! **Per-provider HTTP seam.** rig-core has no batch path, so each provider is
//! hand-rolled behind the [`BatchProvider`] trait — the same shape as the
//! [`ImageGen`](crate::image_gen) seam (one trait, per-kind impls, honest refusal
//! where absent). This slice ships **Anthropic Message Batches** ([`AnthropicBatch`]),
//! whose requests ride inline in one POST; Gemini (its own shape) and OpenAI
//! (file-based) are tracked follow-ons in `docs/issues.md`. A non-Anthropic backend is
//! refused loudly at resolution, never a silent no-op.
//!
//! **No persistent state.** kaibo holds nothing on disk; a batch id *is* the provider's
//! own id, so poll/cancel rebuild a fresh client from the backend and re-address it. A
//! restart drops nothing the provider still has — the design the `docs/issues.md` entry
//! commits to.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::config::{Backend, Defaults, ModelRole, ModelSlot, SlotTunables};
use crate::credentials::ProviderKind;

/// Anthropic's API base. The keyed Anthropic backend carries no `base_url` (rig fixes
/// its endpoint), so batch addresses the service directly.
const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

/// The effort batch runs every item at — the maxed knob. Equal to
/// [`crate::consult::DEFAULT_EFFORT`] today (the proven-accepted top for the Anthropic
/// adaptive tier); kept a separate constant so batch's "spend, it's async" intent
/// survives a change to the interactive default. Forced regardless of the slot's
/// configured `effort` — a cast tuned down for flaky interactive use still batches hot.
pub const BATCH_EFFORT: &str = "high";

/// Floor for a batch item's completion budget. Thinking eats the completion budget, so
/// a thin `max_tokens` starves the answer; batch is async and cheap, so it never skimps.
/// Floored (not fixed) so a slot that *already* asks for more keeps it.
pub const BATCH_MAX_TOKENS_FLOOR: u64 = 1 << 15; // 32768

/// Thinking budget for the budget-style tiers (older Anthropic / Haiku). Kept under
/// [`BATCH_MAX_TOKENS_FLOOR`] so reasoning never starves the answer (Anthropic also
/// *requires* `budget_tokens < max_tokens`). Ignored by the adaptive tier, which
/// expresses depth as `output_config.effort` rather than a token budget.
pub const BATCH_THINKING_BUDGET: u64 = 1 << 14; // 16384

/// One question to run in a batch — toolless by construction. `custom_id` is the
/// provider's per-item correlation key (kaibo assigns the item's index); `prompt` is
/// the whole of the model's input, the same as a `oneshot` prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    pub custom_id: String,
    pub prompt: String,
}

/// One finished item's outcome: the model's text, or a reason it didn't produce one
/// (the provider errored, canceled, or expired the request). Per-item, so one failed
/// question never sinks the rest of the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchAnswer {
    pub custom_id: String,
    /// `Ok(text)` for a succeeded request; `Err(reason)` for an errored/canceled/
    /// expired one — surfaced honestly per item, never silently dropped.
    pub text: Result<String, String>,
}

/// Where a batch is in its lifecycle, as one poll sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchPoll {
    /// Still running. `completed`/`total` drive a human-readable progress line.
    Pending { completed: u64, total: u64 },
    /// A cancel is in flight; results aren't ready yet.
    Cancelling,
    /// Finished — every item's outcome, succeeded or not.
    Done(Vec<BatchAnswer>),
}

/// One batch as the provider's list endpoint reports it — enough to re-address and
/// triage it without fetching results. The orphan-recovery view: kaibo holds no state,
/// so when a caller has lost a handle, listing the provider's own batches is the only
/// way back to it. `provider_id` rebuilds the handle (`backend/provider_id`); `status`
/// is the raw `processing_status`; `created_at` (when present) helps pick the one you
/// meant out of several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchListItem {
    pub provider_id: String,
    pub status: String,
    pub completed: u64,
    pub total: u64,
    pub created_at: Option<String>,
}

/// A provider's batch lane: submit a fan-out, poll it, cancel it, list what's there.
/// One method each, so the whole path is exercised offline against [`ScriptedBatch`]
/// and the concrete provider is swappable as more batch backends land — the
/// [`ImageGen`](crate::image_gen) discipline.
#[async_trait]
pub trait BatchProvider: Send + Sync {
    /// Submit `items` (each answered under the shared `system` preamble) as one batch;
    /// returns the provider's batch id.
    async fn submit(&self, system: &str, items: &[BatchItem]) -> Result<String>;
    /// Poll a batch by its provider id.
    async fn poll(&self, batch_id: &str) -> Result<BatchPoll>;
    /// Ask the provider to cancel a batch by its id.
    async fn cancel(&self, batch_id: &str) -> Result<()>;
    /// List the most recent batches the provider still knows about (newest first),
    /// plus whether more exist beyond the page (so a truncated view is never silent).
    async fn list(&self) -> Result<(Vec<BatchListItem>, bool)>;
}

// --- Request shaping (pure, offline-testable) ------------------------------

/// Batch's maxed request shape for one (kind, model): the floored completion budget and
/// the forced thinking block, at [`BATCH_EFFORT`]. Threads `BATCH_EFFORT` *explicitly*
/// through [`ModelShape::to_params`](crate::consult::ModelShape::to_params) rather than
/// the public [`thinking_params`](crate::consult::thinking_params) helper (which would
/// bake in `consult`'s interactive `DEFAULT_EFFORT`) — so batch's effort stays decoupled
/// from the interactive default, the way the [`BATCH_EFFORT`] doc promises.
///
/// Every knob is *floored*, never capped: `max_tokens` and the thinking budget both rise
/// to the batch floor but keep a slot's already-higher value — that's the "max the knobs"
/// promise (batch spends even when the cast was tuned thin for interactive use), and it
/// can't *undercut* a slot that already asks for more. The budget is held strictly under
/// `max_tokens` (Anthropic requires `budget_tokens < max_tokens`; the adaptive tier
/// ignores the budget and expresses depth as `output_config.effort`). Sampling is left
/// to the provider default — `None`/`None` here — since batch maxes thinking, and the
/// adaptive tier rejects sampling under thinking anyway.
pub fn batch_shaping(
    kind: ProviderKind,
    model: &str,
    tunables: &SlotTunables,
) -> (u64, Option<Value>) {
    let max_tokens = tunables.max_tokens.max(BATCH_MAX_TOKENS_FLOOR);
    // Floor the thinking budget (never undercut a slot that already asks for more), held
    // strictly under max_tokens so reasoning never starves the answer.
    let budget = tunables
        .thinking_budget
        .max(BATCH_THINKING_BUDGET)
        .min(max_tokens.saturating_sub(1));
    let params = crate::consult::ModelShape::resolve(kind, model, tunables.thinking_style)
        .to_params(budget, None, None, BATCH_EFFORT);
    (max_tokens, params)
}

/// Build the Anthropic Message Batches request body. Each item becomes one request
/// whose `params` is a Messages API body: `model`/`max_tokens`/`system`/`messages` plus
/// the flattened thinking block (`thinking`, and the adaptive tier's `output_config`).
/// Pure so the wire shape — including the knob-maxing merged in from `params` — is
/// pinned without a network.
pub fn anthropic_batch_body(
    model: &str,
    max_tokens: u64,
    system: &str,
    params: &Option<Value>,
    items: &[BatchItem],
) -> Value {
    let requests: Vec<Value> = items
        .iter()
        .map(|it| {
            let mut p = Map::new();
            p.insert("model".into(), json!(model));
            p.insert("max_tokens".into(), json!(max_tokens));
            p.insert("system".into(), json!(system));
            p.insert(
                "messages".into(),
                json!([{ "role": "user", "content": it.prompt }]),
            );
            // Flatten the thinking/effort block in, the way rig flattens
            // additional_params into a Messages body (consult.rs).
            if let Some(Value::Object(extra)) = params {
                for (k, v) in extra {
                    p.insert(k.clone(), v.clone());
                }
            }
            json!({ "custom_id": it.custom_id, "params": Value::Object(p) })
        })
        .collect();
    json!({ "requests": requests })
}

// --- Response parsing (pure, offline-testable) -----------------------------

/// What a status poll tells us before we go fetch results.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StatusKind {
    Pending { completed: u64, total: u64 },
    Cancelling,
    Ended { results_url: String },
}

/// Pull the batch id out of a submit/status response. A missing `id` is a broken
/// contract worth surfacing, not papering over.
fn parse_batch_id(v: &Value) -> Result<String> {
    v.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("batch response has no string `id`: {v}"))
}

/// Read a batch's `request_counts` into `(completed, total)`. `completed` sums the
/// terminal buckets (succeeded/errored/canceled/expired); `total` adds the still-
/// `processing` count. Shared by the status poll and the list view so both count a
/// batch's progress the same way. A missing block reads as zero rather than erroring —
/// a batch with no counts yet is simply 0/0.
fn counts_of(v: &Value) -> (u64, u64) {
    let c = v.get("request_counts");
    let get = |k: &str| {
        c.and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let processing = get("processing");
    let completed = get("succeeded") + get("errored") + get("canceled") + get("expired");
    (completed, completed + processing)
}

/// Read a batch's `processing_status` + `request_counts` into a [`StatusKind`].
fn parse_status(v: &Value) -> Result<StatusKind> {
    let status = v
        .get("processing_status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch status has no `processing_status`: {v}"))?;
    match status {
        "in_progress" => {
            let (completed, total) = counts_of(v);
            Ok(StatusKind::Pending { completed, total })
        }
        "canceling" => Ok(StatusKind::Cancelling),
        "ended" => {
            let results_url = v
                .get("results_url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("ended batch has no `results_url`: {v}"))?
                .to_string();
            Ok(StatusKind::Ended { results_url })
        }
        other => Err(anyhow!("unknown batch processing_status {other:?}")),
    }
}

/// Parse one batch object from the list endpoint into a [`BatchListItem`]. A missing
/// `id`/`processing_status` is a broken contract, surfaced rather than papered over;
/// counts and `created_at` are best-effort (a batch may not carry them yet).
fn parse_list_item(v: &Value) -> Result<BatchListItem> {
    let provider_id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch list item has no `id`: {v}"))?
        .to_string();
    let status = v
        .get("processing_status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch list item has no `processing_status`: {v}"))?
        .to_string();
    let (completed, total) = counts_of(v);
    let created_at = v
        .get("created_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(BatchListItem {
        provider_id,
        status,
        completed,
        total,
        created_at,
    })
}

/// Parse the list endpoint's `{ data: [...], has_more }` page into items (in the order
/// the provider returned them — newest first) plus the `has_more` flag, so a caller
/// learns a truncated page is truncated instead of mistaking it for the whole set.
fn parse_batch_list(v: &Value) -> Result<(Vec<BatchListItem>, bool)> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("batch list response has no `data` array: {v}"))?;
    let items = data
        .iter()
        .map(parse_list_item)
        .collect::<Result<Vec<_>>>()?;
    let has_more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
    Ok((items, has_more))
}

/// Join the `text` parts of an Anthropic message's content array.
fn message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Parse the JSONL results body: one `{custom_id, result}` per line. A `succeeded`
/// result yields the message text; `errored`/`canceled`/`expired` yield a per-item
/// `Err(reason)` so a single bad item is reported, never silently dropped.
fn parse_results_jsonl(body: &str) -> Result<Vec<BatchAnswer>> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).with_context(|| format!("parsing result line {line:?}"))?;
        let custom_id = v
            .get("custom_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("result line has no `custom_id`: {line}"))?
            .to_string();
        let result = v
            .get("result")
            .ok_or_else(|| anyhow!("result line has no `result`: {line}"))?;
        let kind = result.get("type").and_then(Value::as_str).unwrap_or("");
        let text = match kind {
            "succeeded" => {
                let message = result
                    .get("message")
                    .ok_or_else(|| anyhow!("succeeded result has no `message`: {line}"))?;
                Ok(message_text(message))
            }
            "errored" => {
                // Prefer the human `error.message`, then its `type`, then the raw blob —
                // a readable reason beats dumping the whole JSON object at the caller.
                let err = result.get("error");
                let detail = err
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        err.and_then(|e| e.get("type"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .or_else(|| err.map(|e| e.to_string()))
                    .unwrap_or_else(|| "unknown error".into());
                Err(format!("provider error: {detail}"))
            }
            "canceled" => Err("canceled".into()),
            "expired" => Err("expired before it ran".into()),
            other => Err(format!("unexpected result type {other:?}")),
        };
        out.push(BatchAnswer { custom_id, text });
    }
    Ok(out)
}

// --- Rendering -------------------------------------------------------------

/// Render a poll for the calling agent. Pending shows progress; Done lists each item's
/// answer (or its per-item failure) under a self-labelling footer naming who ran it —
/// the provenance the `docs/issues.md` entry wants on a batch result.
pub fn render_poll(poll: &BatchPoll, label: &str) -> String {
    match poll {
        BatchPoll::Pending { completed, total } => {
            format!("Batch in progress — {completed}/{total} requests done. No need to wait on it — go do other work and `batch_get` this id again later (it can take minutes to hours; the handle keeps).")
        }
        BatchPoll::Cancelling => {
            "Batch is being canceled; poll again for the final per-item results.".to_string()
        }
        BatchPoll::Done(answers) => {
            let mut s = format!("Batch complete — {} result(s):\n", answers.len());
            for a in answers {
                match &a.text {
                    Ok(text) => s.push_str(&format!("\n## [{}]\n{}\n", a.custom_id, text)),
                    Err(reason) => {
                        s.push_str(&format!("\n## [{}] — failed: {}\n", a.custom_id, reason))
                    }
                }
            }
            s.push_str(&format!("\n———\nkaibo · batch · {label}"));
            s
        }
    }
}

/// Render a `batch_list` result for the calling agent. Each entry is a ready-to-use
/// `(handle, item)`; `errors` are per-backend failures (a backend with no key or an
/// unreachable endpoint is reported, not silently skipped — one bad backend never
/// hides the rest); `truncated` names backends whose page hit `has_more` (so a partial
/// view announces itself rather than reading as complete).
pub fn render_list(
    entries: &[(String, BatchListItem)],
    errors: &[(String, String)],
    truncated: &[String],
) -> String {
    let mut s = String::new();
    if entries.is_empty() && errors.is_empty() {
        return "No batches found. Submit one with `batch_submit`.".to_string();
    }
    s.push_str(&format!("Batches — {} found:\n", entries.len()));
    for (handle, it) in entries {
        s.push_str(&format!(
            "\n- `{}` — {}, {}/{} done",
            handle, it.status, it.completed, it.total
        ));
        if let Some(created) = &it.created_at {
            s.push_str(&format!(" (created {created})"));
        }
    }
    if !entries.is_empty() {
        s.push_str("\n\nPoll one with `batch_get <handle>`; cancel with `batch_cancel <handle>`.");
    }
    if !truncated.is_empty() {
        s.push_str(&format!(
            "\n\nMore batches exist beyond this page on backend(s) {} — narrow by polling \
             a specific handle if you don't see the one you want.",
            truncated.join(", ")
        ));
    }
    for (backend, err) in errors {
        s.push_str(&format!("\n\nCould not list backend `{backend}`: {err}"));
    }
    s
}

// --- Anthropic provider ----------------------------------------------------

/// Anthropic Message Batches over HTTP. Holds the connection (client, key) plus the
/// per-submit shaping (model, floored `max_tokens`, the maxed thinking block). Poll and
/// cancel need only the connection, so a [`from_backend`](Self::from_backend) client
/// (no model) drives those after a restart.
pub struct AnthropicBatch {
    http: reqwest::Client,
    api_key: String,
    /// Submit-only: empty on a poll/cancel-only client.
    model: String,
    max_tokens: u64,
    params: Option<Value>,
}

impl AnthropicBatch {
    /// Refuse a non-Anthropic backend loudly: this slice has no batch path for the
    /// other protocols, so attaching one would only fail (or worse, no-op) at call
    /// time. Honest absence beats a false promise — the [`ImageGen`](crate::image_gen)
    /// posture.
    fn require_anthropic(backend: &Backend) -> Result<()> {
        if backend.kind != ProviderKind::Anthropic {
            return Err(anyhow!(
                "batch is only available on anthropic-kind backends today; backend {:?} \
                 is {:?}. Gemini and OpenAI batch are tracked follow-ons (docs/issues.md) \
                 — point the cast's synth slot at an Anthropic backend to batch now.",
                backend.name,
                backend.kind
            ));
        }
        Ok(())
    }

    /// A reqwest client carrying the backend's per-request deadline. Same TLS wiring as
    /// every other client build site: install ring before `.build()`.
    fn http_client(backend: &Backend) -> Result<reqwest::Client> {
        crate::tls::ensure_crypto_provider();
        reqwest::Client::builder()
            .timeout(backend.request_timeout)
            .connect_timeout(backend.request_timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|e| anyhow!("http client init: {e}"))
    }

    /// A submit-capable client: the cast's synth slot, shaped to batch's maxed knobs.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot, defaults: &Defaults) -> Result<Self> {
        Self::require_anthropic(backend)?;
        let tunables = slot.tunables(ModelRole::Synth, defaults);
        let (max_tokens, params) = batch_shaping(backend.kind, &slot.id, &tunables);
        Ok(Self {
            http: Self::http_client(backend)?,
            api_key: backend.resolve_key()?,
            model: slot.id.clone(),
            max_tokens,
            params,
        })
    }

    /// A poll/cancel-only client (no model): all that's needed to re-address a batch by
    /// its id after a restart.
    pub fn from_backend(backend: &Backend) -> Result<Self> {
        Self::require_anthropic(backend)?;
        Ok(Self {
            http: Self::http_client(backend)?,
            api_key: backend.resolve_key()?,
            model: String::new(),
            max_tokens: 0,
            params: None,
        })
    }

    /// Apply the shared Anthropic headers (auth + API version) to a request.
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
    }

    /// GET a results JSONL body. The `results_url` is absolute (from the status), so it
    /// addresses the service directly.
    async fn fetch_results(&self, results_url: &str) -> Result<Vec<BatchAnswer>> {
        let resp = self
            .auth(self.http.get(results_url))
            .send()
            .await
            .map_err(|e| anyhow!("batch results GET failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading batch results body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("batch results GET {status}: {body}"));
        }
        parse_results_jsonl(&body)
    }
}

#[async_trait]
impl BatchProvider for AnthropicBatch {
    async fn submit(&self, system: &str, items: &[BatchItem]) -> Result<String> {
        if self.model.is_empty() {
            return Err(anyhow!(
                "this batch client was built for poll/cancel only (no model) — submit \
                 needs a synth slot"
            ));
        }
        if items.is_empty() {
            return Err(anyhow!("a batch needs at least one prompt"));
        }
        let body = anthropic_batch_body(&self.model, self.max_tokens, system, &self.params, items);
        let url = format!("{ANTHROPIC_API_BASE}/v1/messages/batches");
        let resp = self
            .auth(self.http.post(&url))
            .body(serde_json::to_vec(&body)?)
            .send()
            .await
            .map_err(|e| anyhow!("batch submit POST failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading batch submit body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("batch submit {status}: {text}"));
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing batch submit response: {text}"))?;
        parse_batch_id(&v)
    }

    async fn poll(&self, batch_id: &str) -> Result<BatchPoll> {
        let url = format!("{ANTHROPIC_API_BASE}/v1/messages/batches/{batch_id}");
        let resp = self
            .auth(self.http.get(&url))
            .send()
            .await
            .map_err(|e| anyhow!("batch status GET failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading batch status body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("batch status GET {status}: {text}"));
        }
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing batch status: {text}"))?;
        match parse_status(&v)? {
            StatusKind::Pending { completed, total } => Ok(BatchPoll::Pending { completed, total }),
            StatusKind::Cancelling => Ok(BatchPoll::Cancelling),
            StatusKind::Ended { results_url } => {
                Ok(BatchPoll::Done(self.fetch_results(&results_url).await?))
            }
        }
    }

    async fn cancel(&self, batch_id: &str) -> Result<()> {
        let url = format!("{ANTHROPIC_API_BASE}/v1/messages/batches/{batch_id}/cancel");
        let resp = self
            .auth(self.http.post(&url))
            .send()
            .await
            .map_err(|e| anyhow!("batch cancel POST failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("batch cancel POST {status}: {text}"));
        }
        Ok(())
    }

    async fn list(&self) -> Result<(Vec<BatchListItem>, bool)> {
        // One page at the API's max (100), newest-first by default. `has_more` rides
        // back so a truncated view announces itself — orphan recovery wants the recent
        // tail, and a caller who needs deeper history polls a known handle directly.
        let url = format!("{ANTHROPIC_API_BASE}/v1/messages/batches?limit=100");
        let resp = self
            .auth(self.http.get(&url))
            .send()
            .await
            .map_err(|e| anyhow!("batch list GET failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading batch list body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("batch list GET {status}: {text}"));
        }
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing batch list: {text}"))?;
        parse_batch_list(&v)
    }
}

/// Build a submit-capable provider from a resolved cast slot + backend (refuses a
/// non-Anthropic backend honestly).
pub fn anthropic_submit(
    backend: &Backend,
    slot: &ModelSlot,
    defaults: &Defaults,
) -> Result<Arc<dyn BatchProvider>> {
    Ok(Arc::new(AnthropicBatch::from_slot(
        backend, slot, defaults,
    )?))
}

/// Build a poll/cancel-only provider from a resolved backend.
pub fn anthropic_poll(backend: &Backend) -> Result<Arc<dyn BatchProvider>> {
    Ok(Arc::new(AnthropicBatch::from_backend(backend)?))
}

#[cfg(test)]
mod test_double {
    use super::*;
    use std::sync::Mutex;

    /// A scripted [`BatchProvider`] for offline tests: records submits and replays a
    /// fixed sequence of poll outcomes (so a test can drive submit → pending → done
    /// with no network) — the batch analogue of
    /// [`ScriptedImageGen`](crate::image_gen::ScriptedImageGen).
    pub struct ScriptedBatch {
        submit_id: String,
        polls: Mutex<std::collections::VecDeque<BatchPoll>>,
        submits: Mutex<Vec<(String, Vec<BatchItem>)>>,
        canceled: Mutex<Vec<String>>,
        listing: (Vec<BatchListItem>, bool),
    }

    impl ScriptedBatch {
        /// Returns `submit_id` on submit, then yields `polls` in order (the last is
        /// repeated once the queue drains, so over-polling a Done batch stays Done).
        pub fn new(submit_id: &str, polls: Vec<BatchPoll>) -> Self {
            Self {
                submit_id: submit_id.to_string(),
                polls: Mutex::new(polls.into()),
                submits: Mutex::new(Vec::new()),
                canceled: Mutex::new(Vec::new()),
                listing: (Vec::new(), false),
            }
        }

        /// Seed what `list` returns (items + `has_more`).
        pub fn with_listing(mut self, items: Vec<BatchListItem>, has_more: bool) -> Self {
            self.listing = (items, has_more);
            self
        }

        /// The `(system, items)` of each submit, in order.
        pub fn submits(&self) -> Vec<(String, Vec<BatchItem>)> {
            self.submits.lock().expect("submits lock").clone()
        }

        /// The batch ids cancel was called with.
        pub fn canceled(&self) -> Vec<String> {
            self.canceled.lock().expect("canceled lock").clone()
        }
    }

    #[async_trait]
    impl BatchProvider for ScriptedBatch {
        async fn submit(&self, system: &str, items: &[BatchItem]) -> Result<String> {
            self.submits
                .lock()
                .expect("submits lock")
                .push((system.to_string(), items.to_vec()));
            Ok(self.submit_id.clone())
        }

        async fn poll(&self, _batch_id: &str) -> Result<BatchPoll> {
            let mut q = self.polls.lock().expect("polls lock");
            // Drain in order; repeat the last so an extra poll of a Done batch is Done.
            if q.len() > 1 {
                Ok(q.pop_front().expect("non-empty"))
            } else {
                Ok(q.front()
                    .cloned()
                    .ok_or_else(|| anyhow!("no scripted polls"))?)
            }
        }

        async fn cancel(&self, batch_id: &str) -> Result<()> {
            self.canceled
                .lock()
                .expect("canceled lock")
                .push(batch_id.to_string());
            Ok(())
        }

        async fn list(&self) -> Result<(Vec<BatchListItem>, bool)> {
            Ok(self.listing.clone())
        }
    }
}

#[cfg(test)]
pub use test_double::ScriptedBatch;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, ModelSlot};

    fn anthropic_backend() -> Backend {
        Backend {
            name: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout: Duration::from_secs(30),
        }
    }

    fn tunables(effort: &str, max_tokens: u64) -> SlotTunables {
        SlotTunables {
            max_tokens,
            thinking_budget: 1024,
            temperature: 1.0,
            top_p: 1.0,
            effort: effort.to_string(),
            thinking_style: crate::consult::ThinkingStyleOverride::Auto,
        }
    }

    /// Batch maxes the knobs: even a slot tuned thin for interactive use (tiny
    /// max_tokens, low effort) is floored and forced to the batch effort. The adaptive
    /// tier expresses that as `output_config.effort: "high"`.
    #[test]
    fn shaping_floors_tokens_and_forces_high_effort_adaptive() {
        let (max_tokens, params) = batch_shaping(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            &tunables("low", 100),
        );
        assert!(
            max_tokens >= BATCH_MAX_TOKENS_FLOOR,
            "a thin slot must be floored to the batch budget, got {max_tokens}"
        );
        let params = params.expect("an adaptive Anthropic model carries a thinking block");
        assert_eq!(params["thinking"]["type"], "adaptive");
        // Pin the value to BATCH_EFFORT itself, not the literal "high" — this is what
        // keeps batch's effort decoupled from consult's interactive DEFAULT_EFFORT (the
        // dead-constant bug the cross-family review caught).
        assert_eq!(
            params["output_config"]["effort"], BATCH_EFFORT,
            "batch must force BATCH_EFFORT regardless of the slot's configured effort: {params}"
        );
    }

    /// The thinking budget is a *floor*, not a cap: a budget-tier slot already asking
    /// for more than BATCH_THINKING_BUDGET keeps its higher value (still under
    /// max_tokens). "Max the knobs" means batch never undercuts a richer slot.
    #[test]
    fn shaping_floors_thinking_budget_without_undercutting() {
        // A Haiku slot (budget tier) configured with a budget above the batch floor but
        // below the token floor.
        let high_budget = BATCH_THINKING_BUDGET + 4096;
        let mut t = tunables("high", 100);
        t.thinking_budget = high_budget;
        let (max_tokens, params) = batch_shaping(ProviderKind::Anthropic, "claude-haiku-4-5", &t);
        let params = params.expect("haiku carries a budget thinking block");
        assert_eq!(
            params["thinking"]["budget_tokens"].as_u64().unwrap(),
            high_budget,
            "a slot's already-higher thinking budget must be kept, not capped: {params}"
        );
        assert!(params["thinking"]["budget_tokens"].as_u64().unwrap() < max_tokens);
    }

    /// A budget-tier model (Haiku) carries the enabled/budget thinking block, still
    /// floored on tokens.
    #[test]
    fn shaping_budget_tier_emits_budget_thinking() {
        let (max_tokens, params) = batch_shaping(
            ProviderKind::Anthropic,
            "claude-haiku-4-5",
            &tunables("high", 100),
        );
        assert!(max_tokens >= BATCH_MAX_TOKENS_FLOOR);
        let params = params.expect("haiku carries an enabled/budget thinking block");
        assert_eq!(params["thinking"]["type"], "enabled");
        assert!(
            params["thinking"]["budget_tokens"].as_u64().unwrap() < max_tokens,
            "thinking budget must stay under max_tokens or Anthropic 400s: {params}"
        );
    }

    /// The request body carries one request per item, each a Messages body with the
    /// shared system, the per-item prompt, and the merged-in thinking knobs.
    #[test]
    fn body_merges_system_prompt_and_knobs_per_item() {
        let (max_tokens, params) = batch_shaping(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            &tunables("high", 0),
        );
        let items = vec![
            BatchItem {
                custom_id: "0".into(),
                prompt: "first".into(),
            },
            BatchItem {
                custom_id: "1".into(),
                prompt: "second".into(),
            },
        ];
        let body =
            anthropic_batch_body("claude-sonnet-4-6", max_tokens, "be terse", &params, &items);
        let reqs = body["requests"].as_array().expect("requests array");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0]["custom_id"], "0");
        assert_eq!(reqs[0]["params"]["model"], "claude-sonnet-4-6");
        assert_eq!(reqs[0]["params"]["system"], "be terse");
        assert_eq!(reqs[0]["params"]["messages"][0]["content"], "first");
        // The maxed thinking knob is flattened into each request's params.
        assert_eq!(reqs[0]["params"]["output_config"]["effort"], "high");
        assert_eq!(reqs[1]["params"]["messages"][0]["content"], "second");
    }

    #[test]
    fn parse_id_reads_id_or_errors() {
        assert_eq!(
            parse_batch_id(&json!({ "id": "msgbatch_01ABC" })).unwrap(),
            "msgbatch_01ABC"
        );
        assert!(parse_batch_id(&json!({ "no": "id" })).is_err());
    }

    #[test]
    fn status_in_progress_counts() {
        let v = json!({
            "processing_status": "in_progress",
            "request_counts": { "processing": 7, "succeeded": 2, "errored": 1, "canceled": 0, "expired": 0 }
        });
        assert_eq!(
            parse_status(&v).unwrap(),
            StatusKind::Pending {
                completed: 3,
                total: 10
            }
        );
    }

    #[test]
    fn status_ended_carries_results_url() {
        let v = json!({ "processing_status": "ended", "results_url": "https://x/results" });
        assert_eq!(
            parse_status(&v).unwrap(),
            StatusKind::Ended {
                results_url: "https://x/results".into()
            }
        );
    }

    #[test]
    fn status_canceling_and_unknown() {
        assert_eq!(
            parse_status(&json!({ "processing_status": "canceling" })).unwrap(),
            StatusKind::Cancelling
        );
        assert!(parse_status(&json!({ "processing_status": "wat" })).is_err());
        // An ended batch with no results_url is a broken contract, surfaced loudly.
        assert!(parse_status(&json!({ "processing_status": "ended" })).is_err());
    }

    #[test]
    fn results_jsonl_succeeded_and_failed_per_item() {
        let body = concat!(
            r#"{"custom_id":"0","result":{"type":"succeeded","message":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}}}"#,
            "\n",
            r#"{"custom_id":"1","result":{"type":"errored","error":{"type":"overloaded"}}}"#,
            "\n",
            r#"{"custom_id":"2","result":{"type":"expired"}}"#,
            "\n",
        );
        let answers = parse_results_jsonl(body).unwrap();
        assert_eq!(answers.len(), 3);
        assert_eq!(
            answers[0],
            BatchAnswer {
                custom_id: "0".into(),
                text: Ok("hello world".into())
            }
        );
        assert!(matches!(&answers[1].text, Err(m) if m.contains("overloaded")));
        assert!(matches!(&answers[2].text, Err(m) if m.contains("expired")));
    }

    /// A non-Anthropic backend is refused at construction — this slice has no batch
    /// path for the other protocols. Honest absence, the ImageGen posture.
    #[test]
    fn non_anthropic_backend_refused() {
        for kind in [
            ProviderKind::DeepSeek,
            ProviderKind::Gemini,
            ProviderKind::Openai,
        ] {
            let mut b = anthropic_backend();
            b.kind = kind;
            b.name = format!("{kind:?}").to_lowercase();
            let slot = ModelSlot::bare(&b.name, "some-model");
            let err = AnthropicBatch::from_slot(&b, &slot, &Defaults::default())
                .err()
                .unwrap_or_else(|| panic!("non-anthropic batch ({kind:?}) must be refused"));
            assert!(
                err.to_string().contains("only available on anthropic-kind"),
                "error should explain the anthropic-only constraint: {err}"
            );
        }
    }

    #[test]
    fn list_parses_items_and_has_more() {
        let v = json!({
            "data": [
                {
                    "id": "msgbatch_01AAA",
                    "processing_status": "in_progress",
                    "created_at": "2026-06-22T00:00:00Z",
                    "request_counts": { "processing": 3, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0 }
                },
                {
                    "id": "msgbatch_01BBB",
                    "processing_status": "ended",
                    "request_counts": { "processing": 0, "succeeded": 2, "errored": 0, "canceled": 0, "expired": 0 }
                }
            ],
            "has_more": true
        });
        let (items, has_more) = parse_batch_list(&v).unwrap();
        assert!(has_more);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].provider_id, "msgbatch_01AAA");
        assert_eq!(items[0].status, "in_progress");
        assert_eq!((items[0].completed, items[0].total), (1, 4));
        assert_eq!(items[0].created_at.as_deref(), Some("2026-06-22T00:00:00Z"));
        // An ended batch with no `processing` still counts its terminal buckets.
        assert_eq!((items[1].completed, items[1].total), (2, 2));
        assert_eq!(items[1].created_at, None);
        // A list item missing `id` is a broken contract, surfaced.
        assert!(parse_batch_list(&json!({ "data": [{ "processing_status": "ended" }] })).is_err());
        // A response with no `data` array is surfaced too.
        assert!(parse_batch_list(&json!({ "has_more": false })).is_err());
    }

    /// The list render names each batch by its ready-to-use handle, surfaces a
    /// truncated page, and reports a per-backend failure instead of hiding it.
    #[test]
    fn render_list_shows_handles_truncation_and_errors() {
        let entries = vec![
            (
                "anthropic/msgbatch_01AAA".to_string(),
                BatchListItem {
                    provider_id: "msgbatch_01AAA".into(),
                    status: "in_progress".into(),
                    completed: 1,
                    total: 4,
                    created_at: Some("2026-06-22T00:00:00Z".into()),
                },
            ),
            (
                "anthropic/msgbatch_01BBB".to_string(),
                BatchListItem {
                    provider_id: "msgbatch_01BBB".into(),
                    status: "ended".into(),
                    completed: 2,
                    total: 2,
                    created_at: None,
                },
            ),
        ];
        let out = render_list(
            &entries,
            &[("claude2".to_string(), "no API key".to_string())],
            &["anthropic".to_string()],
        );
        assert!(out.contains("Batches — 2 found"));
        assert!(out.contains("`anthropic/msgbatch_01AAA` — in_progress, 1/4 done"));
        assert!(out.contains("(created 2026-06-22T00:00:00Z)"));
        assert!(out.contains("`anthropic/msgbatch_01BBB` — ended, 2/2 done"));
        assert!(out.contains("More batches exist beyond this page"));
        assert!(out.contains("Could not list backend `claude2`: no API key"));
        // Nothing anywhere → a clear empty message, not a bare header.
        assert_eq!(
            render_list(&[], &[], &[]),
            "No batches found. Submit one with `batch_submit`."
        );
    }

    /// The scripted provider serves a seeded listing.
    #[tokio::test]
    async fn scripted_list_returns_seeded_items() {
        let provider = ScriptedBatch::new("x", vec![]).with_listing(
            vec![BatchListItem {
                provider_id: "msgbatch_01AAA".into(),
                status: "ended".into(),
                completed: 1,
                total: 1,
                created_at: None,
            }],
            false,
        );
        let (items, has_more) = provider.list().await.unwrap();
        assert!(!has_more);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].provider_id, "msgbatch_01AAA");
    }

    /// Done renders each item under a self-labelling footer; a failed item shows its
    /// reason rather than vanishing.
    #[test]
    fn render_done_lists_items_and_footer() {
        let poll = BatchPoll::Done(vec![
            BatchAnswer {
                custom_id: "0".into(),
                text: Ok("the answer".into()),
            },
            BatchAnswer {
                custom_id: "1".into(),
                text: Err("expired before it ran".into()),
            },
        ]);
        let out = render_poll(&poll, "anthropic · claude-sonnet-4-6");
        assert!(out.contains("the answer"));
        assert!(out.contains("[1] — failed: expired"));
        assert!(out.contains("kaibo · batch · anthropic · claude-sonnet-4-6"));
    }

    /// The scripted provider drives a full submit → pending → done flow offline.
    #[tokio::test]
    async fn scripted_submit_poll_flow() {
        let provider = ScriptedBatch::new(
            "msgbatch_test",
            vec![
                BatchPoll::Pending {
                    completed: 0,
                    total: 2,
                },
                BatchPoll::Done(vec![BatchAnswer {
                    custom_id: "0".into(),
                    text: Ok("done".into()),
                }]),
            ],
        );
        let items = vec![BatchItem {
            custom_id: "0".into(),
            prompt: "q".into(),
        }];
        let id = provider.submit("sys", &items).await.unwrap();
        assert_eq!(id, "msgbatch_test");
        assert_eq!(provider.submits()[0].0, "sys");
        assert!(matches!(
            provider.poll(&id).await.unwrap(),
            BatchPoll::Pending { .. }
        ));
        assert!(matches!(
            provider.poll(&id).await.unwrap(),
            BatchPoll::Done(_)
        ));
        // Over-polling a Done batch stays Done.
        assert!(matches!(
            provider.poll(&id).await.unwrap(),
            BatchPoll::Done(_)
        ));
        provider.cancel(&id).await.unwrap();
        assert_eq!(provider.canceled(), vec!["msgbatch_test".to_string()]);
    }
}
