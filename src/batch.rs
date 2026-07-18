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
//! hand-rolled behind the [`BatchProvider`] trait (one trait, per-kind impls, honest
//! refusal where absent). Two protocols ship: **Anthropic Message Batches** ([`AnthropicBatch`]),
//! whose requests ride inline in one POST, and **Gemini batch** ([`GeminiBatch`]), whose
//! inline requests nest under `input_config.requests` and whose results come back inline
//! in the batch's long-running-operation object (no separate results URL). OpenAI batch
//! (file-based) is a tracked follow-on in `docs/issues.md`. An unsupported backend kind
//! is refused loudly at resolution ([`submitter`]/[`poller`]), never a silent no-op.
//!
//! **The provider owns batch state.** A batch id *is* the provider's own id, so
//! poll/cancel rebuild a fresh client from the backend and re-address it, and a restart
//! drops nothing the provider still has — the provider's list endpoint is the source of
//! truth for a batch's state. Persistence adds only a *durable memory of what kaibo
//! submitted*: when `[persistence]` is on, `batch_submit` records the `backend/provider-id`
//! handle (+ label) in the state db so `job_list` can recover it after a restart even
//! before re-querying the provider (see `src/store.rs`, `server::job_list`). That record
//! is bookkeeping, never the authority on batch state.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::attach::Attachment;
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
    /// The whole batch reached a terminal *non-success* state with no per-item results
    /// to hand back (a Gemini cancel/fail/expire, which is instant-terminal rather than
    /// Anthropic's interim `Cancelling`). `state` is the provider's raw state string;
    /// `message` is its reason. Distinct from `Done(vec![])` — that would render as
    /// "0 results" and read like success; this names the failure honestly.
    Failed { state: String, message: String },
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
/// and the concrete provider is swappable as more batch backends land.
#[async_trait]
pub trait BatchProvider: Send + Sync {
    /// Submit `items` (each answered under the shared `system` preamble) as one batch;
    /// returns the provider's batch id. `attachments` are shared workspace files inlined
    /// as context ahead of *every* item's prompt — text spliced inline, images carried as
    /// native base64 parts (see [`crate::attach`]).
    async fn submit(
        &self,
        system: &str,
        attachments: &[Attachment],
        items: &[BatchItem],
    ) -> Result<String>;
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

/// Build one Anthropic message `content` for a prompt plus the shared attachments.
/// With no attachments it stays a plain string (the unchanged wire shape — a bare
/// prompt). With attachments it becomes a content-block array: the attachments first
/// (as *context* — a text file as a `<file>`-wrapped text block, an image as a base64
/// `image` block, Anthropic recommending image-before-text), then the prompt last.
fn anthropic_content(attachments: &[Attachment], prompt: &str) -> Value {
    if attachments.is_empty() {
        return json!(prompt);
    }
    let mut blocks: Vec<Value> = attachments
        .iter()
        .map(|att| match att {
            Attachment::Text { .. } => {
                json!({ "type": "text", "text": att.wrapped_text().expect("text attachment wraps") })
            }
            Attachment::Image { mime, data_b64, .. } => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": mime, "data": data_b64 }
            }),
        })
        .collect();
    blocks.push(json!({ "type": "text", "text": prompt }));
    Value::Array(blocks)
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
    attachments: &[Attachment],
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
                json!([{ "role": "user", "content": anthropic_content(attachments, &it.prompt) }]),
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

/// Parse an RFC3339 timestamp to Unix epoch seconds — the shapes our batch providers
/// emit (Anthropic `2026-06-22T15:40:08.480837+00:00`, Gemini
/// `2026-06-24T15:53:10.062475197Z`). `chrono` (already in the tree via kaish-kernel /
/// rmcp / schemars) handles the offset and fractional seconds; we only *parse* with it
/// and read "now" from `SystemTime`, so its `clock` feature stays off and no new
/// transitive dep rides in. Returns `None` on anything it can't read, so the `list`
/// recency filter *keeps* (never silently drops) an item it can't date.
pub fn rfc3339_to_epoch(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
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

/// Compose one batch item's answer from its assembled text and a *normalized* completion
/// signal — the shared gate both providers pass their finish reason through. `Ok(())` is a
/// clean finish; `Err(detail)` names an abnormal one (a truncation or policy halt). The
/// point is that an abnormal finish is never presented as a clean completion (GH #75):
///
/// - clean + text → the answer as-is,
/// - clean + empty → an honest per-item failure (a completed-but-empty answer is no answer),
/// - abnormal + empty → a per-item failure naming the finish reason (e.g. the model spent
///   its whole budget thinking and never reached an answer),
/// - abnormal + text → the *partial* text kept under a loud INCOMPLETE banner, so a caller
///   skimming for findings can't mistake a clipped fragment (often trailing reasoning, with
///   none of the requested structure or verdict) for a finished answer. The text is kept,
///   not dropped — a genuinely-clipped real answer stays useful — but it announces itself.
fn finish_gated_answer(text: String, gate: Result<(), String>) -> Result<String, String> {
    match gate {
        Ok(()) => {
            if text.trim().is_empty() {
                Err("model returned an empty answer".to_string())
            } else {
                Ok(text)
            }
        }
        Err(detail) => {
            if text.trim().is_empty() {
                Err(format!("model produced no answer — {detail}"))
            } else {
                Ok(format!(
                    "⚠️ INCOMPLETE — {detail}\nThe text below is a truncated fragment, not a \
                     finished answer (it may be trailing reasoning and is missing any requested \
                     structure or verdict).\n\n{text}"
                ))
            }
        }
    }
}

/// Gate an Anthropic message's `stop_reason`. A toolless batch item finishes cleanly on
/// `end_turn` / `stop_sequence`; `max_tokens` is the silent-truncation case this guards
/// against, and any other value (a refusal, an unknown) is likewise abnormal. An absent
/// reason (a real succeeded message always carries one) is treated as clean — the
/// empty-text check still catches a genuinely empty response.
fn anthropic_finish_gate(stop_reason: Option<&str>) -> Result<(), String> {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") | None => Ok(()),
        Some(reason) => {
            let note = match reason {
                "max_tokens" => {
                    "the response hit its output-token budget before finishing; \
                    re-run with a higher max_tokens or a narrower prompt"
                }
                "refusal" => "the model declined to complete this request",
                _ => "the response did not complete normally",
            };
            Err(format!("stop_reason={reason}: {note}"))
        }
    }
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
                let stop = message.get("stop_reason").and_then(Value::as_str);
                finish_gated_answer(message_text(message), anthropic_finish_gate(stop))
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
            format!("Batch in progress — {completed}/{total} requests done. No need to wait on it — go do other work and `job_get` this handle again later (it can take minutes to hours; the handle keeps).")
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
        BatchPoll::Failed { state, message } => {
            format!("Batch ended in {state} — {message}. No per-item results to return.")
        }
    }
}

/// Render the batches section of `list` for the calling agent. Each entry is a ready-to-use
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
        s.push_str("\n\nPoll one with `job_get <handle>`; cancel with `job_cancel <handle>`.");
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
    /// time. Honest absence beats a false promise.
    fn require_anthropic(backend: &Backend) -> Result<()> {
        if backend.kind != ProviderKind::Anthropic {
            return Err(anyhow!(
                "this is the Anthropic batch provider, but backend {:?} is {:?}. \
                 (Batch dispatch should never route a non-Anthropic backend here; see \
                 `submitter`/`poller`.)",
                backend.name,
                backend.kind
            ));
        }
        Ok(())
    }

    /// A submit-capable client: the cast's synth slot, shaped to batch's maxed knobs.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot, defaults: &Defaults) -> Result<Self> {
        Self::require_anthropic(backend)?;
        let tunables = slot.tunables(ModelRole::Synth, defaults);
        let (max_tokens, params) = batch_shaping(backend.kind, &slot.id, &tunables);
        Ok(Self {
            http: batch_http_client(backend)?,
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
            http: batch_http_client(backend)?,
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
    async fn submit(
        &self,
        system: &str,
        attachments: &[Attachment],
        items: &[BatchItem],
    ) -> Result<String> {
        if self.model.is_empty() {
            return Err(anyhow!(
                "this batch client was built for poll/cancel only (no model) — submit \
                 needs a synth slot"
            ));
        }
        if items.is_empty() {
            return Err(anyhow!("a batch needs at least one prompt"));
        }
        let body = anthropic_batch_body(
            &self.model,
            self.max_tokens,
            system,
            &self.params,
            attachments,
            items,
        );
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

// --- Gemini provider -------------------------------------------------------

/// Gemini's API base. The keyed Gemini backend carries no `base_url` (rig fixes its
/// endpoint), so batch addresses the service directly, as Anthropic does.
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// A reqwest client carrying the backend's per-request deadline — the shared
/// [`crate::tls::https_client`] is the one build site (ring installed, `rustls-no-provider`,
/// no OpenSSL/C); this just supplies the backend's timeout. Used by both batch providers.
fn batch_http_client(backend: &Backend) -> Result<reqwest::Client> {
    crate::tls::https_client(backend.request_timeout)
}

/// Read a (possibly string-encoded) count field. Gemini's `batchStats` numbers arrive as
/// JSON *strings* (`"1"`), not numbers — `as_u64` alone would silently read them as 0,
/// the kind of quiet miscount the project forbids. A missing/garbage field is 0.
fn gemini_num(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Progress `(completed, total)` from a batch's `metadata.batchStats`. `completed` sums
/// the terminal buckets (succeeded + failed); `total` is `requestCount`. Shared by the
/// status poll and the list view so both count progress the same way.
fn gemini_progress(meta: Option<&Value>) -> (u64, u64) {
    let stats = meta.and_then(|m| m.get("batchStats"));
    let n = |k: &str| stats.map(|s| gemini_num(s, k)).unwrap_or(0);
    (
        n("successfulRequestCount") + n("failedRequestCount"),
        n("requestCount"),
    )
}

/// The answer text (non-thought parts joined) plus the candidate's `finishReason`.
/// Separating the two lets result assembly gate on the finish reason instead of presenting
/// an incomplete fragment as a clean answer (GH #75). With `includeThoughts` on, the model's
/// reasoning rides as separate `"thought": true` parts — the scratchpad, not the answer — so
/// the answer is the text parts that are *not* thoughts. A finished answer part may still
/// carry a `thoughtSignature`: under a clean `STOP` that's a real answer, so it stays; under
/// a truncated finish (`MAX_TOKENS`) it's trailing reasoning that lost its `thought` marker,
/// which the finish-reason gate catches — the signal we can't get from the parts alone.
fn gemini_candidate(response: &Value) -> (String, Option<String>) {
    let cand = response
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let text = cand
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish = cand
        .and_then(|c| c.get("finishReason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (text, finish)
}

/// Gate a Gemini `finishReason`. `STOP` is a clean completion; an absent reason (a real
/// candidate always carries one) is treated as clean too. Every other value is abnormal —
/// `MAX_TOKENS` is the common one for attached-file reviews at max thinking (the exact
/// GH #75 case: the whole output budget spent thinking, no answer reached), so its note
/// points at the fix; the policy halts name themselves.
fn gemini_finish_gate(finish: Option<&str>) -> Result<(), String> {
    match finish {
        Some("STOP") | None => Ok(()),
        Some(reason) => {
            let note = match reason {
                "MAX_TOKENS" => {
                    "the response hit its output-token budget before finishing — \
                    common when a big attached-file review spends the budget thinking; re-run \
                    with a higher max_tokens or a narrower prompt"
                }
                "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "RECITATION" | "SPII" => {
                    "the provider halted generation on a content policy"
                }
                _ => "the response did not complete normally",
            };
            Err(format!("finishReason={reason}: {note}"))
        }
    }
}

/// Locate a batch's inlined per-item responses. Gemini puts them in the long-running
/// operation's `response.inlinedResponses.inlinedResponses` once done; the same array is
/// mirrored under `metadata.output.inlinedResponses.inlinedResponses`, so we accept
/// either (the LRO result first).
fn gemini_inlined_array(v: &Value) -> Option<&Vec<Value>> {
    fn at(root: &Value) -> Option<&Vec<Value>> {
        root.get("inlinedResponses")
            .and_then(|i| i.get("inlinedResponses"))
            .and_then(Value::as_array)
    }
    v.get("response")
        .and_then(at)
        .or_else(|| v.get("metadata").and_then(|m| m.get("output")).and_then(at))
}

/// Parse the inlined-responses array into per-item [`BatchAnswer`]s. Each element carries
/// its `metadata.key` plus either a `response` (the model's content) or an `error`
/// (surfaced per item, never silently dropped — the Anthropic per-item ethos).
fn parse_gemini_inlined(arr: &[Value]) -> Vec<BatchAnswer> {
    arr.iter()
        .map(|el| {
            let custom_id = el
                .get("metadata")
                .and_then(|m| m.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let text = if let Some(resp) = el.get("response") {
                let (answer, finish) = gemini_candidate(resp);
                finish_gated_answer(answer, gemini_finish_gate(finish.as_deref()))
            } else if let Some(err) = el.get("error") {
                let detail = err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string());
                Err(format!("provider error: {detail}"))
            } else {
                Err("no response or error in result item".to_string())
            };
            BatchAnswer { custom_id, text }
        })
        .collect()
}

/// Pull the batch's operation `name` (`batches/<id>`) out of a submit/status response.
fn parse_gemini_name(v: &Value) -> Result<String> {
    v.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("gemini batch response has no string `name`: {v}"))
}

/// Map a status response onto a [`BatchPoll`]. The `state` enum drives it: pending/running
/// → progress; succeeded → the inlined results; cancelled/failed/expired → either the
/// partial results that *did* land before the terminal event, or (none) a [`Failed`] that
/// names the state honestly rather than a misleading empty `Done`.
///
/// [`Failed`]: BatchPoll::Failed
fn parse_gemini_poll(v: &Value) -> Result<BatchPoll> {
    let meta = v.get("metadata");
    let state = meta
        .and_then(|m| m.get("state"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("gemini batch has no `metadata.state`: {v}"))?;
    match state {
        "BATCH_STATE_PENDING" | "BATCH_STATE_RUNNING" => {
            let (completed, total) = gemini_progress(meta);
            Ok(BatchPoll::Pending { completed, total })
        }
        "BATCH_STATE_SUCCEEDED" => {
            let arr = gemini_inlined_array(v).ok_or_else(|| {
                anyhow!(
                    "succeeded gemini batch has no inlined responses — a file-based output \
                     (responsesFile) isn't supported; kaibo only submits inline batches: {v}"
                )
            })?;
            Ok(BatchPoll::Done(parse_gemini_inlined(arr)))
        }
        "BATCH_STATE_CANCELLED" | "BATCH_STATE_FAILED" | "BATCH_STATE_EXPIRED" => {
            // Items that finished before the terminal event still carry results — hand
            // those back rather than throwing them away.
            if let Some(arr) = gemini_inlined_array(v) {
                if !arr.is_empty() {
                    return Ok(BatchPoll::Done(parse_gemini_inlined(arr)));
                }
            }
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "batch did not complete".to_string());
            Ok(BatchPoll::Failed {
                state: state.to_string(),
                message,
            })
        }
        other => Err(anyhow!("unknown gemini batch state {other:?}: {v}")),
    }
}

/// Parse one operation from the list endpoint into a [`BatchListItem`]. The operation
/// `name` (`batches/<id>`) is the provider id; status is the raw `metadata.state`.
fn parse_gemini_list_item(op: &Value) -> Result<BatchListItem> {
    let provider_id = op
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("gemini batch list item has no `name`: {op}"))?
        .to_string();
    let meta = op.get("metadata");
    let status = meta
        .and_then(|m| m.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("BATCH_STATE_UNSPECIFIED")
        .to_string();
    let (completed, total) = gemini_progress(meta);
    let created_at = meta
        .and_then(|m| m.get("createTime"))
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

/// Parse the list endpoint's `{ operations: [...], nextPageToken }` page. A `nextPageToken`
/// means more exist (so a truncated view announces itself); an absent `operations` key is
/// an empty page (the endpoint omits it when there are no batches), not an error.
fn parse_gemini_list(v: &Value) -> Result<(Vec<BatchListItem>, bool)> {
    let items = match v.get("operations").and_then(Value::as_array) {
        Some(ops) => ops
            .iter()
            .map(parse_gemini_list_item)
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    let has_more = v
        .get("nextPageToken")
        .and_then(Value::as_str)
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    Ok((items, has_more))
}

/// Build the Gemini inline-batch request body. Each item becomes one
/// `{ request: GenerateContentRequest, metadata: { key } }`: the shared `system` rides as
/// `systemInstruction`, the prompt as a single user `content`, and `maxOutputTokens` is
/// merged into the `generationConfig` the maxed thinking block already produced (Gemini
/// nests the completion budget *and* `thinkingConfig` there, unlike Anthropic's top-level
/// `max_tokens` — so the bodies can't be shared). Pure so the wire shape is pinned without
/// a network.
pub fn gemini_batch_body(
    max_tokens: u64,
    system: &str,
    params: &Option<Value>,
    attachments: &[Attachment],
    items: &[BatchItem],
) -> Value {
    let requests: Vec<Value> = items
        .iter()
        .map(|it| {
            // Start from the shaping's generationConfig (the thinking block) and fold the
            // floored completion budget in beside it.
            let mut gen = params
                .as_ref()
                .and_then(|p| p.get("generationConfig"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            gen.insert("maxOutputTokens".into(), json!(max_tokens));
            let mut req = Map::new();
            if !system.is_empty() {
                req.insert(
                    "systemInstruction".into(),
                    json!({ "parts": [{ "text": system }] }),
                );
            }
            req.insert(
                "contents".into(),
                json!([{ "role": "user", "parts": gemini_parts(attachments, &it.prompt) }]),
            );
            req.insert("generationConfig".into(), Value::Object(gen));
            json!({ "request": Value::Object(req), "metadata": { "key": it.custom_id } })
        })
        .collect();
    json!({
        "batch": {
            "display_name": "kaibo-batch",
            "input_config": { "requests": { "requests": requests } }
        }
    })
}

/// Build one Gemini user-turn `parts` array for a prompt plus the shared attachments:
/// the attachments first as context (a text file as a `<file>`-wrapped text part, an
/// image as an `inlineData` part), then the prompt last. Always an array (Gemini's
/// native part shape), so the no-attachment case is just `[{ text: prompt }]` — the
/// unchanged wire shape.
fn gemini_parts(attachments: &[Attachment], prompt: &str) -> Value {
    let mut parts: Vec<Value> = attachments
        .iter()
        .map(|att| match att {
            Attachment::Text { .. } => {
                json!({ "text": att.wrapped_text().expect("text attachment wraps") })
            }
            Attachment::Image { mime, data_b64, .. } => {
                json!({ "inlineData": { "mimeType": mime, "data": data_b64 } })
            }
        })
        .collect();
    parts.push(json!({ "text": prompt }));
    Value::Array(parts)
}

/// Gemini batch over HTTP. Mirrors [`AnthropicBatch`]: holds the connection (client, key,
/// base) plus the per-submit shaping (model, floored `max_tokens`, the maxed thinking
/// block). Poll/cancel/list need only the connection, so a [`from_backend`](Self::from_backend)
/// client (no model) drives those after a restart.
pub struct GeminiBatch {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Submit-only: empty on a poll/cancel-only client.
    model: String,
    max_tokens: u64,
    params: Option<Value>,
}

impl GeminiBatch {
    /// Refuse a non-Gemini backend loudly — dispatch ([`submitter`]/[`poller`]) should
    /// never route one here, so this is a belt-and-suspenders contract guard.
    fn require_gemini(backend: &Backend) -> Result<()> {
        if backend.kind != ProviderKind::Gemini {
            return Err(anyhow!(
                "this is the Gemini batch provider, but backend {:?} is {:?}. (Batch \
                 dispatch should never route a non-Gemini backend here; see \
                 `submitter`/`poller`.)",
                backend.name,
                backend.kind
            ));
        }
        Ok(())
    }

    fn base_url(backend: &Backend) -> String {
        backend
            .base_url
            .clone()
            .unwrap_or_else(|| GEMINI_API_BASE.to_string())
    }

    /// A submit-capable client: the cast's synth slot, shaped to batch's maxed knobs.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot, defaults: &Defaults) -> Result<Self> {
        Self::require_gemini(backend)?;
        let tunables = slot.tunables(ModelRole::Synth, defaults);
        let (max_tokens, params) = batch_shaping(backend.kind, &slot.id, &tunables);
        Ok(Self {
            http: batch_http_client(backend)?,
            api_key: backend.resolve_key()?,
            base_url: Self::base_url(backend),
            model: slot.id.clone(),
            max_tokens,
            params,
        })
    }

    /// A poll/cancel/list-only client (no model).
    pub fn from_backend(backend: &Backend) -> Result<Self> {
        Self::require_gemini(backend)?;
        Ok(Self {
            http: batch_http_client(backend)?,
            api_key: backend.resolve_key()?,
            base_url: Self::base_url(backend),
            model: String::new(),
            max_tokens: 0,
            params: None,
        })
    }

    /// GET/POST a JSON object, returning the parsed body or a loud error with the raw text.
    async fn send_json(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let resp = req
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .send()
            .await
            .map_err(|e| anyhow!("gemini batch {what} failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading gemini batch {what} body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("gemini batch {what} {status}: {text}"));
        }
        serde_json::from_str(&text).with_context(|| format!("parsing gemini batch {what}: {text}"))
    }
}

#[async_trait]
impl BatchProvider for GeminiBatch {
    async fn submit(
        &self,
        system: &str,
        attachments: &[Attachment],
        items: &[BatchItem],
    ) -> Result<String> {
        if self.model.is_empty() {
            return Err(anyhow!(
                "this batch client was built for poll/cancel only (no model) — submit \
                 needs a synth slot"
            ));
        }
        if items.is_empty() {
            return Err(anyhow!("a batch needs at least one prompt"));
        }
        let body = gemini_batch_body(self.max_tokens, system, &self.params, attachments, items);
        let url = format!(
            "{}/models/{}:batchGenerateContent",
            self.base_url, self.model
        );
        let v = self
            .send_json(
                self.http.post(&url).body(serde_json::to_vec(&body)?),
                "submit",
            )
            .await?;
        parse_gemini_name(&v)
    }

    async fn poll(&self, batch_id: &str) -> Result<BatchPoll> {
        let url = format!("{}/{}", self.base_url, batch_id);
        let v = self.send_json(self.http.get(&url), "status").await?;
        parse_gemini_poll(&v)
    }

    async fn cancel(&self, batch_id: &str) -> Result<()> {
        let url = format!("{}/{}:cancel", self.base_url, batch_id);
        self.send_json(self.http.post(&url), "cancel").await?;
        Ok(())
    }

    async fn list(&self) -> Result<(Vec<BatchListItem>, bool)> {
        // One page at the API's max (100), newest-first by default; `nextPageToken` rides
        // back as `has_more` so a truncated view announces itself (orphan recovery wants
        // the recent tail).
        let url = format!("{}/batches?pageSize=100", self.base_url);
        let v = self.send_json(self.http.get(&url), "list").await?;
        parse_gemini_list(&v)
    }
}

// --- Dispatch --------------------------------------------------------------

/// Does this provider kind have a batch lane? The single source of truth — dispatch
/// ([`submitter`]/[`poller`]), `list`'s backend filter, and refusal messages all
/// defer to it, so adding a provider is a one-line change here. Keep the `matches!` arm in
/// step with the per-kind impls below.
pub fn batch_supported(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::Anthropic | ProviderKind::Gemini)
}

/// The batch-capable kinds as a display list, derived from [`batch_supported`] so a refusal
/// names the live set without a hand-maintained string that drifts as providers are added.
pub fn supported_kinds_list() -> String {
    [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::OpenRouter,
        ProviderKind::Openai,
    ]
    .into_iter()
    .filter(|k| batch_supported(*k))
    .map(ProviderKind::canonical_name)
    .collect::<Vec<_>>()
    .join(", ")
}

/// The refusal for an unsupported backend kind — shared by [`submitter`] and [`poller`] so
/// the message stays in one place. Names the live supported set via [`supported_kinds_list`].
fn unsupported(backend: &Backend) -> anyhow::Error {
    anyhow!(
        "backend {:?} ({:?}) has no batch lane. Point the cast's synth slot at a \
         batch-capable backend ({}); `kaibo://config` lists the casts.",
        backend.name,
        backend.kind,
        supported_kinds_list()
    )
}

/// Build a submit-capable provider from a resolved cast slot + backend, dispatching on the
/// backend kind. An unsupported kind is refused honestly.
pub fn submitter(
    backend: &Backend,
    slot: &ModelSlot,
    defaults: &Defaults,
) -> Result<Arc<dyn BatchProvider>> {
    match backend.kind {
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicBatch::from_slot(
            backend, slot, defaults,
        )?)),
        ProviderKind::Gemini => Ok(Arc::new(GeminiBatch::from_slot(backend, slot, defaults)?)),
        _ => Err(unsupported(backend)),
    }
}

/// Build a poll/cancel/list-only provider from a resolved backend, dispatching on kind.
pub fn poller(backend: &Backend) -> Result<Arc<dyn BatchProvider>> {
    match backend.kind {
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicBatch::from_backend(backend)?)),
        ProviderKind::Gemini => Ok(Arc::new(GeminiBatch::from_backend(backend)?)),
        _ => Err(unsupported(backend)),
    }
}

/// The seam that lets the batch *handlers* be tested offline. `KaiboHandler` holds one of
/// these and builds every provider through it, so a test can swap the real
/// network-client builders ([`submitter`]/[`poller`]) for a double returning a
/// [`ScriptedBatch`] — closing the handler-level coverage gap that the free-function
/// call sites left (the consult side already injects via `Arm::new`). The two methods
/// mirror the free functions exactly; production is [`LiveBatchProviders`].
pub trait BatchProviderFactory: Send + Sync {
    /// A model-bearing provider for submitting a batch (mirrors [`submitter`]).
    fn submitter(
        &self,
        backend: &Backend,
        slot: &ModelSlot,
        defaults: &Defaults,
    ) -> Result<Arc<dyn BatchProvider>>;

    /// A poll/cancel/list-only provider (mirrors [`poller`]).
    fn poller(&self, backend: &Backend) -> Result<Arc<dyn BatchProvider>>;
}

/// The production factory: the real `submitter`/`poller`, building network clients. The
/// default a server runs on; only tests substitute another.
pub struct LiveBatchProviders;

impl BatchProviderFactory for LiveBatchProviders {
    fn submitter(
        &self,
        backend: &Backend,
        slot: &ModelSlot,
        defaults: &Defaults,
    ) -> Result<Arc<dyn BatchProvider>> {
        submitter(backend, slot, defaults)
    }

    fn poller(&self, backend: &Backend) -> Result<Arc<dyn BatchProvider>> {
        poller(backend)
    }
}

#[cfg(test)]
mod test_double {
    use super::*;
    use std::sync::Mutex;

    /// One recorded submit: the shared `(system, attachments, items)` a test asserts on.
    type SubmitRecord = (String, Vec<Attachment>, Vec<BatchItem>);

    /// A scripted [`BatchProvider`] for offline tests: records submits and replays a
    /// fixed sequence of poll outcomes (so a test can drive submit → pending → done
    /// with no network).
    pub struct ScriptedBatch {
        submit_id: String,
        polls: Mutex<std::collections::VecDeque<BatchPoll>>,
        submits: Mutex<Vec<SubmitRecord>>,
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

        /// The `(system, attachments, items)` of each submit, in order.
        pub fn submits(&self) -> Vec<SubmitRecord> {
            self.submits.lock().expect("submits lock").clone()
        }

        /// The batch ids cancel was called with.
        pub fn canceled(&self) -> Vec<String> {
            self.canceled.lock().expect("canceled lock").clone()
        }
    }

    #[async_trait]
    impl BatchProvider for ScriptedBatch {
        async fn submit(
            &self,
            system: &str,
            attachments: &[Attachment],
            items: &[BatchItem],
        ) -> Result<String> {
            self.submits.lock().expect("submits lock").push((
                system.to_string(),
                attachments.to_vec(),
                items.to_vec(),
            ));
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

    /// A [`BatchProviderFactory`] that hands the *same* [`ScriptedBatch`] to every
    /// submit and poll — so a handler test drives one double across a batch's whole
    /// lifecycle (submit here, then `job_get`/`job_cancel`/`job_list` re-address it).
    /// The backend/slot/defaults are ignored; the double is pre-scripted.
    pub struct ScriptedBatchProviders(pub Arc<ScriptedBatch>);

    impl BatchProviderFactory for ScriptedBatchProviders {
        fn submitter(
            &self,
            _backend: &Backend,
            _slot: &ModelSlot,
            _defaults: &Defaults,
        ) -> Result<Arc<dyn BatchProvider>> {
            Ok(self.0.clone())
        }

        fn poller(&self, _backend: &Backend) -> Result<Arc<dyn BatchProvider>> {
            Ok(self.0.clone())
        }
    }
}

#[cfg(test)]
pub use test_double::{ScriptedBatch, ScriptedBatchProviders};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::config::{Defaults, ModelSlot};

    #[test]
    fn rfc3339_epoch_known_anchors() {
        // The Unix epoch itself, and a hand-verified anchor (2000-01-01 = 946684800).
        assert_eq!(rfc3339_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_epoch("2000-01-01T00:00:00Z"), Some(946_684_800));
    }

    #[test]
    fn rfc3339_epoch_handles_provider_shapes() {
        // Anthropic (`+00:00`, microseconds) and Gemini (`Z`, nanoseconds) — the
        // fraction is ignored, both are UTC, so each equals its whole-second instant.
        assert_eq!(
            rfc3339_to_epoch("2026-06-22T15:40:08.480837+00:00"),
            rfc3339_to_epoch("2026-06-22T15:40:08Z"),
        );
        assert_eq!(
            rfc3339_to_epoch("2026-06-24T15:53:10.062475197Z"),
            rfc3339_to_epoch("2026-06-24T15:53:10Z"),
        );
    }

    #[test]
    fn rfc3339_epoch_applies_the_offset() {
        // 01:00 at +01:00 is the same instant as 00:00 UTC — UTC = civil − offset.
        assert_eq!(
            rfc3339_to_epoch("2000-01-01T01:00:00+01:00"),
            rfc3339_to_epoch("2000-01-01T00:00:00Z"),
        );
        // A negative offset pushes the instant later in UTC.
        assert_eq!(
            rfc3339_to_epoch("2000-01-01T00:00:00-05:00"),
            Some(946_684_800 + 5 * 3600),
        );
    }

    #[test]
    fn rfc3339_epoch_rejects_garbage() {
        // Unparseable input is None, so the recency filter keeps the item rather than
        // silently dropping a batch it couldn't date.
        assert_eq!(rfc3339_to_epoch("not-a-timestamp"), None);
        assert_eq!(rfc3339_to_epoch("2026-06-24"), None);
        assert_eq!(rfc3339_to_epoch(""), None);
    }

    fn anthropic_backend() -> Backend {
        Backend {
            name: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout: Duration::from_secs(30),
            data_collection: Default::default(),
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
        let body = anthropic_batch_body(
            "claude-sonnet-4-6",
            max_tokens,
            "be terse",
            &params,
            &[],
            &items,
        );
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

    /// With shared attachments, each Anthropic item's `content` becomes a block array:
    /// the attachments first (a `<file>`-wrapped text block, then a base64 `image`
    /// block), then the prompt last — the same shared attachments on every item.
    #[test]
    fn anthropic_content_carries_text_and_image_attachments() {
        let attachments = vec![
            Attachment::Text {
                path: "README.md".into(),
                body: "hello".into(),
            },
            Attachment::Image {
                path: "logo.png".into(),
                mime: "image/png",
                data_b64: "QUJD".into(),
            },
        ];
        let items = vec![
            BatchItem {
                custom_id: "0".into(),
                prompt: "review this".into(),
            },
            BatchItem {
                custom_id: "1".into(),
                prompt: "and this".into(),
            },
        ];
        let body = anthropic_batch_body(
            "claude-sonnet-4-6",
            1024,
            "sys",
            &None,
            &attachments,
            &items,
        );
        let content = &body["requests"][0]["params"]["messages"][0]["content"];
        let blocks = content
            .as_array()
            .expect("attachments make content a block array");
        // [text-attachment, image-attachment, prompt]
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("path=\"README.md\""),
            "the text block wraps the file: {}",
            blocks[0]["text"]
        );
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
        // The prompt rides last, after the context.
        assert_eq!(blocks[2]["type"], "text");
        assert_eq!(blocks[2]["text"], "review this");
        // The same shared attachments ride on the second item, ahead of its own prompt.
        let c1 = body["requests"][1]["params"]["messages"][0]["content"]
            .as_array()
            .expect("second item also a block array");
        assert_eq!(c1[2]["text"], "and this");
        assert_eq!(c1[1]["source"]["data"], "QUJD");
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
            r#"{"custom_id":"0","result":{"type":"succeeded","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}}}"#,
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
                // A clean `end_turn` passes through untouched — no banner.
                text: Ok("hello world".into())
            }
        );
        assert!(matches!(&answers[1].text, Err(m) if m.contains("overloaded")));
        assert!(matches!(&answers[2].text, Err(m) if m.contains("expired")));
    }

    /// An Anthropic item that hit `stop_reason: max_tokens` is *not* a clean completion:
    /// its partial text is kept but announces itself under an INCOMPLETE banner naming the
    /// stop reason, so a caller can't mistake a clipped answer for a finished one (GH #75).
    #[test]
    fn results_jsonl_max_tokens_is_flagged_incomplete() {
        let body = concat!(
            r#"{"custom_id":"0","result":{"type":"succeeded","message":{"stop_reason":"max_tokens","content":[{"type":"text","text":"partial finding, cut off mid-"}]}}}"#,
            "\n",
        );
        let answers = parse_results_jsonl(body).unwrap();
        let text = answers[0]
            .text
            .as_ref()
            .expect("partial text kept, not an Err");
        assert!(text.contains("INCOMPLETE"), "banner present: {text}");
        assert!(text.contains("max_tokens"), "names the stop reason: {text}");
        assert!(
            text.contains("partial finding, cut off mid-"),
            "the partial text is kept, not dropped: {text}"
        );
    }

    /// An Anthropic item that truncated before emitting *any* answer text is an honest
    /// per-item failure naming the stop reason — never a silent empty success.
    #[test]
    fn results_jsonl_max_tokens_empty_is_failure() {
        let body = r#"{"custom_id":"0","result":{"type":"succeeded","message":{"stop_reason":"max_tokens","content":[]}}}"#;
        let answers = parse_results_jsonl(body).unwrap();
        assert!(
            matches!(&answers[0].text, Err(m) if m.contains("no answer") && m.contains("max_tokens")),
            "empty + truncated is a failure naming the reason: {:?}",
            answers[0].text
        );
    }

    /// The Anthropic provider refuses any non-Anthropic backend at construction — the
    /// belt-and-suspenders guard behind dispatch (which should never route one here).
    /// Honest absence over a false promise. (Cross-kind *dispatch* refusal is covered by
    /// `dispatch_refuses_unsupported_kinds`.)
    #[test]
    fn anthropic_provider_refuses_non_anthropic() {
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
                err.to_string().contains("Anthropic batch provider"),
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

    // --- Gemini ------------------------------------------------------------

    /// Gemini's maxed shape lands in `generationConfig` (the completion budget nests
    /// there, not top-level): a Gemini slot carries `thinkingConfig.thinkingLevel`, and
    /// `max_tokens` is floored to the batch minimum. The whole 3-line is single-tier, so
    /// batch forces the level and never a token budget.
    #[test]
    fn gemini_shaping_nests_thinking_in_generation_config() {
        let (max_tokens, params) = batch_shaping(
            ProviderKind::Gemini,
            "gemini-3.5-flash",
            &tunables("high", 100),
        );
        assert!(max_tokens >= BATCH_MAX_TOKENS_FLOOR);
        let params = params.expect("a gemini slot carries a thinking block");
        let tc = &params["generationConfig"]["thinkingConfig"];
        assert_eq!(
            tc["thinkingLevel"], BATCH_EFFORT,
            "gemini batch forces the level, nested in generationConfig: {params}"
        );
        assert!(
            tc.get("thinkingBudget").is_none(),
            "Gemini has no budget tier — the level is the only knob: {params}"
        );
    }

    /// A 3-line id takes `thinkingLevel`, forced to BATCH_EFFORT — the per-role effort
    /// lever, not a token budget — even when the slot asked for a shallower "low". (The
    /// `gemini-batch` cast synths `gemini-pro-latest`, also a level-tier id now that the
    /// whole Gemini 3-line takes a level; every Gemini id runs this one path through
    /// `batch_shaping`.)
    #[test]
    fn gemini_3_line_uses_thinking_level_at_batch_effort() {
        let (_max, params) = batch_shaping(
            ProviderKind::Gemini,
            "gemini-3-pro-preview",
            &tunables("low", 100),
        );
        let params = params.expect("a gemini-3 line carries a thinking block");
        assert_eq!(
            params["generationConfig"]["thinkingConfig"]["thinkingLevel"], BATCH_EFFORT,
            "the 3-line forces BATCH_EFFORT as thinkingLevel: {params}"
        );
    }

    /// The body nests maxOutputTokens *beside* the shaping's thinkingConfig in one
    /// generationConfig (Gemini's shape, not Anthropic's top-level max_tokens), carries
    /// the shared system as systemInstruction, and keys each item by its custom_id.
    #[test]
    fn gemini_body_merges_system_maxtokens_and_thinking() {
        let (max_tokens, params) = batch_shaping(
            ProviderKind::Gemini,
            "gemini-3-pro-preview",
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
        let body = gemini_batch_body(max_tokens, "be terse", &params, &[], &items);
        let reqs = body["batch"]["input_config"]["requests"]["requests"]
            .as_array()
            .expect("requests array");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0]["metadata"]["key"], "0");
        assert_eq!(
            reqs[0]["request"]["systemInstruction"]["parts"][0]["text"],
            "be terse"
        );
        assert_eq!(
            reqs[0]["request"]["contents"][0]["parts"][0]["text"],
            "first"
        );
        let gc = &reqs[0]["request"]["generationConfig"];
        // maxOutputTokens and the thinking block coexist in one generationConfig.
        assert_eq!(gc["maxOutputTokens"].as_u64().unwrap(), max_tokens);
        assert_eq!(gc["thinkingConfig"]["thinkingLevel"], "high");
        assert_eq!(reqs[1]["metadata"]["key"], "1");
        assert_eq!(
            reqs[1]["request"]["contents"][0]["parts"][0]["text"],
            "second"
        );
    }

    /// An empty system prompt omits systemInstruction entirely (Gemini rejects an empty
    /// one) rather than sending a blank block.
    #[test]
    fn gemini_body_omits_empty_system() {
        let items = vec![BatchItem {
            custom_id: "0".into(),
            prompt: "q".into(),
        }];
        let body = gemini_batch_body(1024, "", &None, &[], &items);
        let req = &body["batch"]["input_config"]["requests"]["requests"][0]["request"];
        assert!(
            req.get("systemInstruction").is_none(),
            "empty system must be omitted: {req}"
        );
        // maxOutputTokens still lands even with no shaping params.
        assert_eq!(
            req["generationConfig"]["maxOutputTokens"].as_u64().unwrap(),
            1024
        );
    }

    /// With shared attachments, each Gemini item's `parts` array leads with the
    /// attachments as context (a `<file>`-wrapped text part, then an `inlineData` image
    /// part), then the prompt last.
    #[test]
    fn gemini_parts_carry_text_and_image_attachments() {
        let attachments = vec![
            Attachment::Text {
                path: "diff.patch".into(),
                body: "@@ -1 +1 @@".into(),
            },
            Attachment::Image {
                path: "shot.png".into(),
                mime: "image/png",
                data_b64: "QUJD".into(),
            },
        ];
        let items = vec![BatchItem {
            custom_id: "0".into(),
            prompt: "review".into(),
        }];
        let body = gemini_batch_body(1024, "sys", &None, &attachments, &items);
        let parts = body["batch"]["input_config"]["requests"]["requests"][0]["request"]["contents"]
            [0]["parts"]
            .as_array()
            .expect("parts array");
        assert_eq!(parts.len(), 3);
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("path=\"diff.patch\""),
            "the text part wraps the file: {}",
            parts[0]["text"]
        );
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "QUJD");
        assert_eq!(parts[2]["text"], "review");
    }

    #[test]
    fn gemini_name_reads_or_errors() {
        assert_eq!(
            parse_gemini_name(&json!({ "name": "batches/abc123" })).unwrap(),
            "batches/abc123"
        );
        assert!(parse_gemini_name(&json!({ "no": "name" })).is_err());
    }

    /// batchStats counts arrive as JSON *strings*; progress reads them anyway (a number
    /// read as 0 would be a silent miscount). completed = succeeded + failed.
    #[test]
    fn gemini_progress_reads_string_counts() {
        let v = json!({ "metadata": { "batchStats": {
            "requestCount": "10", "pendingRequestCount": "7",
            "successfulRequestCount": "2", "failedRequestCount": "1"
        }}});
        assert_eq!(gemini_progress(v.get("metadata")), (3, 10));
    }

    #[test]
    fn gemini_poll_pending_running_and_unknown() {
        let pend = json!({ "metadata": { "state": "BATCH_STATE_PENDING",
            "batchStats": { "requestCount": "2", "pendingRequestCount": "2" } } });
        assert_eq!(
            parse_gemini_poll(&pend).unwrap(),
            BatchPoll::Pending {
                completed: 0,
                total: 2
            }
        );
        let run = json!({ "metadata": { "state": "BATCH_STATE_RUNNING",
            "batchStats": { "requestCount": "2", "successfulRequestCount": "1" } } });
        assert_eq!(
            parse_gemini_poll(&run).unwrap(),
            BatchPoll::Pending {
                completed: 1,
                total: 2
            }
        );
        // No state, or an unknown one, is a broken contract surfaced loudly.
        assert!(parse_gemini_poll(&json!({ "metadata": {} })).is_err());
        assert!(parse_gemini_poll(&json!({ "metadata": { "state": "WAT" } })).is_err());
    }

    /// A succeeded batch yields per-item answers; thought parts are filtered out of the
    /// text, a final answer part keeping its thoughtSignature stays, and a per-item error
    /// is surfaced rather than dropped.
    #[test]
    fn gemini_poll_succeeded_parses_inlined() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_SUCCEEDED" },
            "response": { "inlinedResponses": { "inlinedResponses": [
                { "metadata": { "key": "0" }, "response": { "candidates": [ {
                    "finishReason": "STOP",
                    "content": { "parts": [
                        { "text": "thinking out loud", "thought": true },
                        { "text": "the ", "thoughtSignature": "sig" },
                        { "text": "answer" }
                    ] } } ] } },
                { "metadata": { "key": "1" }, "error": { "code": 7, "message": "permission denied" } }
            ] } }
        });
        let poll = parse_gemini_poll(&v).unwrap();
        let answers = match poll {
            BatchPoll::Done(a) => a,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].custom_id, "0");
        // Clean STOP: thought part dropped; the two real text parts joined; a final answer
        // part keeping its thoughtSignature stays (a signatured part is a real answer under
        // STOP). No banner.
        assert_eq!(answers[0].text, Ok("the answer".to_string()));
        assert!(matches!(&answers[1].text, Err(m) if m.contains("permission denied")));
    }

    /// GH #75, ground-truthed against the real durable payload
    /// (`gemini/batches/laaxe5t0oa9181hhc1y65282fow9fksnpz6i`): a `MAX_TOKENS` batch item
    /// whose budget was spent thinking. The response has a `thought:true` summary part and a
    /// *second* non-thought part that carries a `thoughtSignature` but is really trailing
    /// reasoning — it starts mid-sentence and never reaches the requested structure. The old
    /// code emitted that fragment as a clean answer; now the finish reason gates it into a
    /// loud INCOMPLETE banner (the partial text kept, but unmistakably not a finished review).
    #[test]
    fn gemini_max_tokens_leaked_reasoning_is_flagged() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_SUCCEEDED" },
            "response": { "inlinedResponses": { "inlinedResponses": [
                { "metadata": { "key": "0" }, "response": { "candidates": [ {
                    "finishReason": "MAX_TOKENS",
                    "content": { "role": "model", "parts": [
                        { "text": "**Comprehensive Lexer Review**\nMy task is to review...", "thought": true },
                        { "text": "has_bracket_pair` is true. The parser will see `Int(1)`, `MinusAlone`.", "thoughtSignature": "Ct8B..." }
                    ] } } ] } }
            ] } }
        });
        let answers = match parse_gemini_poll(&v).unwrap() {
            BatchPoll::Done(a) => a,
            other => panic!("expected Done, got {other:?}"),
        };
        let text = answers[0]
            .text
            .as_ref()
            .expect("partial text kept, not an Err");
        assert!(text.contains("INCOMPLETE"), "banner present: {text}");
        assert!(
            text.contains("MAX_TOKENS"),
            "names the finish reason: {text}"
        );
        // The thought *summary* is still dropped; the leaked-reasoning fragment is what's
        // kept (under the banner) — never presented as a clean answer.
        assert!(
            !text.contains("Comprehensive Lexer Review"),
            "the thought-summary part is still filtered out: {text}"
        );
        assert!(
            text.contains("has_bracket_pair"),
            "the truncated fragment is kept under the banner: {text}"
        );
    }

    /// A `MAX_TOKENS` finish that produced *only* a thought part (no answer text at all) is a
    /// per-item failure naming the reason — "model produced no answer" beats a blank success.
    #[test]
    fn gemini_max_tokens_only_thoughts_is_failure() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_SUCCEEDED" },
            "response": { "inlinedResponses": { "inlinedResponses": [
                { "metadata": { "key": "0" }, "response": { "candidates": [ {
                    "finishReason": "MAX_TOKENS",
                    "content": { "parts": [
                        { "text": "still reasoning about the lexer...", "thought": true }
                    ] } } ] } }
            ] } }
        });
        let answers = match parse_gemini_poll(&v).unwrap() {
            BatchPoll::Done(a) => a,
            other => panic!("expected Done, got {other:?}"),
        };
        assert!(
            matches!(&answers[0].text, Err(m) if m.contains("no answer") && m.contains("MAX_TOKENS")),
            "only-thoughts + truncated is a failure naming the reason: {:?}",
            answers[0].text
        );
    }

    /// The pure finish-gate + compose contract, straight — including the paths the
    /// inlined-parse tests don't reach (a policy halt, a clean-but-empty answer).
    #[test]
    fn finish_gate_and_compose_contract() {
        // Clean finishes pass text through untouched, both providers.
        assert!(gemini_finish_gate(Some("STOP")).is_ok());
        assert!(gemini_finish_gate(None).is_ok());
        assert!(anthropic_finish_gate(Some("end_turn")).is_ok());
        assert!(anthropic_finish_gate(Some("stop_sequence")).is_ok());
        assert_eq!(
            finish_gated_answer("answer".into(), gemini_finish_gate(Some("STOP"))),
            Ok("answer".into())
        );
        // A clean finish with no text is an honest failure, not a blank success.
        assert!(
            matches!(finish_gated_answer(String::new(), Ok(())), Err(m) if m.contains("empty"))
        );
        // A policy halt is abnormal and names itself; an empty one is a per-item failure.
        let safety = gemini_finish_gate(Some("SAFETY"));
        assert!(matches!(&safety, Err(m) if m.contains("SAFETY") && m.contains("policy")));
        assert!(
            matches!(finish_gated_answer(String::new(), safety), Err(m) if m.contains("no answer"))
        );
        // A refusal on the Anthropic side likewise gates into a banner with kept text.
        let out = finish_gated_answer(
            "as far as I got".into(),
            anthropic_finish_gate(Some("refusal")),
        );
        assert!(
            matches!(&out, Ok(t) if t.contains("INCOMPLETE") && t.contains("refusal") && t.contains("as far as I got"))
        );
    }

    /// A cancelled batch with no partial results is a [`BatchPoll::Failed`] naming the
    /// state and reason — not a misleading empty `Done`. (Gemini cancel is instant-
    /// terminal, so there's no `Cancelling` interim like Anthropic's.)
    #[test]
    fn gemini_poll_cancelled_with_no_results_is_failed() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_CANCELLED",
                "batchStats": { "requestCount": "1", "pendingRequestCount": "1" } },
            "error": { "code": 13, "message": "Batch x failed without error." }
        });
        assert_eq!(
            parse_gemini_poll(&v).unwrap(),
            BatchPoll::Failed {
                state: "BATCH_STATE_CANCELLED".into(),
                message: "Batch x failed without error.".into()
            }
        );
    }

    /// A cancelled batch that *did* finish some items before the terminal event hands
    /// those partial results back rather than throwing them away.
    #[test]
    fn gemini_poll_cancelled_with_partials_is_done() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_CANCELLED",
                "output": { "inlinedResponses": { "inlinedResponses": [
                    { "metadata": { "key": "0" }, "response": { "candidates": [ { "content": { "parts": [ { "text": "done in time" } ] } } ] } }
                ] } } }
        });
        match parse_gemini_poll(&v).unwrap() {
            BatchPoll::Done(a) => {
                assert_eq!(a.len(), 1);
                assert_eq!(a[0].text, Ok("done in time".to_string()));
            }
            other => panic!("expected Done with partials, got {other:?}"),
        }
    }

    /// A succeeded batch whose output is a file (not inline) is refused loudly — kaibo only
    /// submits inline batches, so a file output is an unhandled shape, not silent emptiness.
    #[test]
    fn gemini_poll_succeeded_without_inline_errors() {
        let v = json!({ "metadata": { "state": "BATCH_STATE_SUCCEEDED",
            "output": { "responsesFile": "files/abc" } } });
        assert!(parse_gemini_poll(&v).is_err());
    }

    /// The list endpoint reads `operations` (not `data`) and treats `nextPageToken` as the
    /// has-more flag. The operation `name` is the provider id (`batches/<id>`).
    #[test]
    fn gemini_list_parses_operations_and_page_token() {
        let v = json!({
            "operations": [
                { "name": "batches/aaa", "metadata": { "state": "BATCH_STATE_RUNNING",
                    "createTime": "2026-06-22T00:00:00Z",
                    "batchStats": { "requestCount": "4", "successfulRequestCount": "1" } } },
                { "name": "batches/bbb", "metadata": { "state": "BATCH_STATE_SUCCEEDED",
                    "batchStats": { "requestCount": "2", "successfulRequestCount": "2" } } }
            ],
            "nextPageToken": "tok"
        });
        let (items, has_more) = parse_gemini_list(&v).unwrap();
        assert!(has_more);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].provider_id, "batches/aaa");
        assert_eq!(items[0].status, "BATCH_STATE_RUNNING");
        assert_eq!((items[0].completed, items[0].total), (1, 4));
        assert_eq!(items[0].created_at.as_deref(), Some("2026-06-22T00:00:00Z"));
        // An empty list omits `operations` entirely — that's an empty page, not an error.
        let (empty, more) = parse_gemini_list(&json!({})).unwrap();
        assert!(empty.is_empty() && !more);
    }

    /// A non-Gemini backend is refused at GeminiBatch construction — the dispatch guard.
    #[test]
    fn gemini_provider_refuses_non_gemini() {
        let mut b = anthropic_backend();
        b.kind = ProviderKind::Anthropic;
        let slot = ModelSlot::bare(&b.name, "claude-sonnet-4-6");
        let err = GeminiBatch::from_slot(&b, &slot, &Defaults::default())
            .err()
            .expect("non-gemini must be refused");
        assert!(err.to_string().contains("Gemini batch provider"), "{err}");
    }

    /// Dispatch refuses a kind with no batch lane (DeepSeek / local openai) and points at
    /// the supported casts; `batch_supported` agrees with that set.
    #[test]
    fn dispatch_refuses_unsupported_kinds() {
        assert!(batch_supported(ProviderKind::Anthropic));
        assert!(batch_supported(ProviderKind::Gemini));
        assert!(!batch_supported(ProviderKind::DeepSeek));
        assert!(!batch_supported(ProviderKind::OpenRouter));
        assert!(!batch_supported(ProviderKind::Openai));
        for kind in [
            ProviderKind::DeepSeek,
            ProviderKind::OpenRouter,
            ProviderKind::Openai,
        ] {
            let mut b = anthropic_backend();
            b.kind = kind;
            b.name = format!("{kind:?}").to_lowercase();
            let slot = ModelSlot::bare(&b.name, "m");
            let err = submitter(&b, &slot, &Defaults::default())
                .err()
                .unwrap_or_else(|| panic!("{kind:?} must be refused"));
            let msg = err.to_string();
            assert!(
                msg.contains("no batch lane"),
                "refusal explains the gap: {err}"
            );
            // The supported set is named dynamically from `batch_supported`, so the message
            // tracks the live set rather than a hand-maintained string.
            assert!(
                msg.contains("anthropic") && msg.contains("gemini"),
                "refusal names the live supported set: {err}"
            );
            assert!(poller(&b).is_err());
        }
    }

    /// Failed renders a clear terminal line (state + reason), not a "0 results" success.
    #[test]
    fn render_failed_names_state_and_reason() {
        let out = render_poll(
            &BatchPoll::Failed {
                state: "BATCH_STATE_EXPIRED".into(),
                message: "ran past its 24h window".into(),
            },
            "gemini · gemini-3-pro-preview",
        );
        assert!(out.contains("BATCH_STATE_EXPIRED"));
        assert!(out.contains("ran past its 24h window"));
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
        let id = provider.submit("sys", &[], &items).await.unwrap();
        assert_eq!(id, "msgbatch_test");
        assert_eq!(provider.submits()[0].0, "sys");
        assert!(
            provider.submits()[0].1.is_empty(),
            "no attachments in this flow"
        );
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
