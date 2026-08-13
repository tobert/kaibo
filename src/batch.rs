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
//! tool loop. So batch is built on the `oneshot` *shape* (a lone synthesis agent
//! answering from what it was handed), never `consult`.
//!
//! **Floor the knobs by default.** Batch is the cheap/async lane, so it spends: every
//! knob rises to a batch minimum — `max_tokens` to [`BATCH_MAX_TOKENS_FLOOR`], the
//! thinking budget to [`BATCH_THINKING_BUDGET`], reasoning depth to [`BATCH_EFFORT`] —
//! and none of them is ever *capped*. A slot tuned thin for flaky interactive use still
//! batches hot; a slot that already asks for more keeps it. The latency that makes deep
//! thinking painful synchronously is free here — the caller already accepted "come back
//! later." This is the lane for asking the best model the hard question and waiting.
//!
//! **Per-provider HTTP seam.** rig-core has no batch path, so each provider is
//! hand-rolled behind the [`BatchProvider`] trait (one trait, per-provider impls, honest
//! refusal where absent). Three protocols ship: **Anthropic Message Batches**
//! ([`AnthropicBatch`]), whose requests ride inline in one POST; **Gemini batch**
//! ([`GeminiBatch`]), whose inline requests nest under `input_config.requests` and whose
//! results come back inline in the batch's long-running-operation object (no separate
//! results URL); and **OpenAI Batch** ([`OpenaiBatch`]), the file-based one — the JSONL is
//! built in memory and uploaded as a file *in the caller's OpenAI account*, the batch
//! references it, and results are downloaded back as files (kaibo still writes nothing
//! locally). A backend with no batch lane is refused loudly at resolution
//! ([`submitter`]/[`poller`]), never a silent no-op — and for `openai` that verdict is
//! per-*backend*, since only OpenAI Platform serves `/v1/batches` ([`batch_supported`]).
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
use crate::credentials::{ProviderKind, WireKind};

/// Anthropic's default API base, used when the backend sets no `base_url` of its own
/// (the common case — rig fixes the endpoint for most keyed kinds, but the anthropic
/// kind may override it to dial an Anthropic-Messages-API-compatible gateway/proxy;
/// see [`AnthropicBatch::base_url`]).
const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

/// The reasoning depth batch *floors* every item at. Equal to
/// [`crate::consult::DEFAULT_EFFORT`] today — `high` is *observed-accepted* on the
/// Anthropic adaptive tier (every live consult rides it); whether it is that tier's
/// **top** is genuinely unknown, since the probe that would settle it
/// (`anthropic_adaptive_effort_ladder_live`, `tests/consult.rs`) cannot run on an
/// unfunded key: Anthropic's billing check precedes body validation, so a 400 there
/// says nothing about the rung. `docs/config.example.toml` ships `effort = "max"` on an
/// Opus slot on the other assumption; exactly one of these is right, and both stay put
/// until the probe answers. Kept a separate constant either way, so batch's
/// "spend, it's async" intent survives a change to the interactive default.
///
/// A floor, matching every other batch knob — see [`batch_effort`] for why the direction
/// matters and how an unrankable rung is handled.
pub const BATCH_EFFORT: &str = "high";

/// The reasoning depth one batch item runs at: the deeper of the slot's `effort` and
/// [`BATCH_EFFORT`] — a floor on **depth**, which is why the reasoning off-switch is the
/// first thing it asks about.
///
/// Batch floors its knobs rather than fixing them, and effort was the one exception —
/// force-clobbered in *both* directions. Lifting a thin slot is the intent ("a cast tuned
/// down for flaky interactive use still batches hot"); demoting a slot that deliberately
/// asked for `xhigh`/`max` was collateral, and silent. So, in order:
///
/// - the slot is [`EFFORT_OFF`](crate::consult::EFFORT_OFF) → the slot wins and reasoning
///   stays off. **A depth floor has no business switching reasoning on.** `none` is a
///   sentinel, not the shallowest rung of
///   [`EFFORT_LADDER`](crate::consult::EFFORT_LADDER) — it used to sit there, and this
///   function lifted it to `BATCH_EFFORT`, billing reasoning on every item of a fan-out.
///   A cheap bulk-extraction batch is exactly where an operator turns reasoning off, and
///   exactly where the per-item cost multiplies.
/// - the slot ranks **shallower** than the floor → the floor wins (the deliberate lift).
/// - the slot ranks **deeper** → the slot wins (never cap a richer slot, the same promise
///   `max_tokens` and the thinking budget already make).
/// - either value sits **outside** the ladder → the slot wins. kaibo keeps no effort
///   allowlist, so a rung it can't order is an operator reaching for something newer than
///   this table; trusting it is what keeps that passthrough real, and quietly replacing it
///   with "high" would be exactly the override this function exists to retire.
///
/// Equal depths return the slot's own string, spelling included — kaibo never normalizes
/// an operator's value on its way to a provider. A caller deciding whether the configured
/// value "ships as written" must therefore compare by
/// [`effort_rank`](crate::consult::effort_rank), not by string; see
/// [`Config::effort_disposition`](crate::config::Config::effort_disposition), which is
/// the single place that judgement lives.
pub fn batch_effort(slot_effort: &str) -> &str {
    // Off first, and before any ranking: `is_effort_off` answers the question
    // `effort_rank` structurally cannot, which is the whole reason it is public.
    if crate::consult::is_effort_off(slot_effort) {
        return slot_effort;
    }
    match (
        crate::consult::effort_rank(slot_effort),
        crate::consult::effort_rank(BATCH_EFFORT),
    ) {
        (Some(slot), Some(floor)) if slot < floor => BATCH_EFFORT,
        _ => slot_effort,
    }
}

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
    /// Poll a batch by its provider id. `submitted` is how many items kaibo submitted
    /// this batch with — `None` when the caller has no durable record of it (persistence
    /// off, or a handle from before kaibo started recording the count). A finished poll
    /// cross-checks its result set against `submitted` and reports any `custom_id` the
    /// provider never returned as a per-item failure, so a silently dropped item is loud
    /// instead of just missing from the count.
    async fn poll(&self, batch_id: &str, submitted: Option<u64>) -> Result<BatchPoll>;
    /// Ask the provider to cancel a batch by its id.
    async fn cancel(&self, batch_id: &str) -> Result<()>;
    /// List the most recent batches the provider still knows about (newest first),
    /// plus whether more exist beyond the page (so a truncated view is never silent).
    async fn list(&self) -> Result<(Vec<BatchListItem>, bool)>;
}

// --- Request shaping (pure, offline-testable) ------------------------------

/// Batch's request shape for one (kind, model): the floored completion budget and the
/// forced-on thinking block at the floored effort. Threads the effort *explicitly*
/// through [`ModelShape::to_params`](crate::consult::ModelShape::to_params) rather than
/// the public [`thinking_params`](crate::consult::thinking_params) helper (which would
/// bake in `consult`'s interactive `DEFAULT_EFFORT`) — so batch's depth stays decoupled
/// from the interactive default, the way the [`BATCH_EFFORT`] doc promises.
///
/// Every knob is a *floor*, never a cap: `max_tokens`, the thinking budget, and the
/// effort ([`batch_effort`]) each rise to the batch minimum but keep a slot's
/// already-higher value — the "spend, it's async" promise in the direction that helps,
/// without undercutting a cast that deliberately asked for more. The budget is held
/// strictly under `max_tokens` (Anthropic requires `budget_tokens < max_tokens`; the
/// adaptive tier ignores the budget and expresses depth as `output_config.effort`).
/// Sampling is left to the provider default — `None`/`None` here — since batch runs
/// thinking deep, and the adaptive tier rejects sampling under thinking anyway.
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
        .to_params(budget, None, None, batch_effort(&tunables.effort));
    (max_tokens, params)
}

/// The batch-lane analog of the `run_phase` span (`consult/engine.rs`): a `batch_submit`
/// span carrying the resolved request shape, so a trace shows what a batch shipped —
/// `model`, the item count, and `gen_ai.request.thinking` (the forced thinking/effort
/// blob from [`batch_shaping`], e.g. Gemini's `thinkingLevel` at [`BATCH_EFFORT`]). The
/// batch request is assembled outside `run_phase`, so without this the batch lane is
/// wire-invisible where the interactive lane is legible.
///
/// Sync, and returns the span (rather than an `#[instrument]` on the network-bound
/// `submit`) so it owns *both* the field declaration and the record and is fully
/// unit-testable without a live submit; each provider's `submit` holds it for the call's
/// lifetime. Inert with no exporter — a disabled span skips the `%model` format and the
/// `record` is a no-op — and a toggle-less shape (`params == None`) records nothing.
fn batch_request_span(model: &str, params: Option<&Value>, items: usize) -> tracing::Span {
    let span = tracing::info_span!(
        "batch_submit",
        model = %model,
        items = items,
        gen_ai.request.thinking = tracing::field::Empty,
    );
    if let Some(p) = params {
        span.record("gen_ai.request.thinking", tracing::field::display(p));
    }
    span
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

/// The message kaibo attaches to a `custom_id` it submitted but a finished batch never
/// returned — kaibo's own cross-check against the count it sent, not a provider-reported
/// per-item error, so the wording says that explicitly rather than reading like one more
/// `provider error: …` line. Names the id, states what is and is not known (present in the
/// finished result set: no; why the provider left it out: unknown — nothing here invents a
/// reason, and nothing suggests re-polling: the provider already declared this batch
/// finished, so there is no later result still in flight to catch), and gives the one real
/// next step — the provider's own batch dashboard for that id.
fn absentee_answer(custom_id: String) -> BatchAnswer {
    let text = format!(
        "kaibo: item `{custom_id}` is missing — this batch is finished, but the provider \
         returned no answer and no error for a custom_id kaibo submitted. This is kaibo's \
         own count check against what it sent, not a failure the provider reported. Look up \
         `{custom_id}` on the provider's own batch dashboard."
    );
    BatchAnswer {
        custom_id,
        text: Err(text),
    }
}

/// Append a synthetic [`absentee_answer`] for every `custom_id` in `0..submitted` that
/// `answers` doesn't already carry. `submitted` is `None` when there is nothing honest to
/// check against — a handle persisted before kaibo recorded a submitted count, or a result
/// set that is a *known* partial (a cancelled/expired batch's before-the-cutoff items) —
/// and in that case `answers` returns untouched rather than false-flagging a documented gap.
///
/// kaibo assigns every `custom_id` itself, as the contiguous decimal indices
/// `0..submitted` (`batch_submit`'s and `deliberate`'s `items` construction never take an
/// id from the caller), so a bare count fully determines the expected set — no id list
/// needs to ride along on the handle.
fn cross_check_absentees(
    mut answers: Vec<BatchAnswer>,
    submitted: Option<u64>,
) -> Vec<BatchAnswer> {
    let Some(submitted) = submitted else {
        return answers;
    };
    let seen: std::collections::HashSet<String> =
        answers.iter().map(|a| a.custom_id.clone()).collect();
    let absentees = (0..submitted)
        .map(|i| i.to_string())
        .filter(|id| !seen.contains(id))
        .map(absentee_answer);
    answers.extend(absentees);
    answers
}

/// Parse the JSONL results body: one `{custom_id, result}` per line. A `succeeded`
/// result yields the message text; `errored`/`canceled`/`expired` yield a per-item
/// `Err(reason)` so a single bad item is reported, never silently dropped. `submitted` is
/// the count kaibo sent this batch with (`None` for a pre-cross-check handle) — every
/// Anthropic terminal batch carries one line per submitted item regardless of how it ended
/// (errored/canceled/expired items are still lines, not omissions), so the cross-check
/// applies unconditionally here, unlike the providers whose terminal states can carry a
/// genuinely partial result set.
fn parse_results_jsonl(body: &str, submitted: Option<u64>) -> Result<Vec<BatchAnswer>> {
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
    Ok(cross_check_absentees(out, submitted))
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
    base_url: String,
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

    /// The backend's configured `base_url`, if it set one (an Anthropic-Messages-API
    /// -compatible gateway/proxy), else [`ANTHROPIC_API_BASE`] — mirrors
    /// [`GeminiBatch::base_url`].
    fn base_url(backend: &Backend) -> String {
        backend
            .base_url
            .clone()
            .unwrap_or_else(|| ANTHROPIC_API_BASE.to_string())
    }

    /// A submit-capable client: the cast's synth slot, shaped to batch's maxed knobs.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot, defaults: &Defaults) -> Result<Self> {
        Self::require_anthropic(backend)?;
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

    /// A poll/cancel-only client (no model): all that's needed to re-address a batch by
    /// its id after a restart.
    pub fn from_backend(backend: &Backend) -> Result<Self> {
        Self::require_anthropic(backend)?;
        Ok(Self {
            http: batch_http_client(backend)?,
            api_key: backend.resolve_key()?,
            base_url: Self::base_url(backend),
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
    /// addresses the service directly. `submitted` rides through to
    /// [`parse_results_jsonl`]'s cross-check.
    async fn fetch_results(
        &self,
        results_url: &str,
        submitted: Option<u64>,
    ) -> Result<Vec<BatchAnswer>> {
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
        parse_results_jsonl(&body, submitted)
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
        // Record the resolved batch request shape (held for the submit's lifetime) — the
        // batch-lane analog of run_phase's gen_ai.request.thinking.
        let _span = batch_request_span(&self.model, self.params.as_ref(), items.len());
        let body = anthropic_batch_body(
            &self.model,
            self.max_tokens,
            system,
            &self.params,
            attachments,
            items,
        );
        let url = format!("{}/v1/messages/batches", self.base_url);
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

    async fn poll(&self, batch_id: &str, submitted: Option<u64>) -> Result<BatchPoll> {
        let url = format!("{}/v1/messages/batches/{batch_id}", self.base_url);
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
            StatusKind::Ended { results_url } => Ok(BatchPoll::Done(
                self.fetch_results(&results_url, submitted).await?,
            )),
        }
    }

    async fn cancel(&self, batch_id: &str) -> Result<()> {
        let url = format!("{}/v1/messages/batches/{batch_id}/cancel", self.base_url);
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
        let url = format!("{}/v1/messages/batches?limit=100", self.base_url);
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

/// Gemini's default API base, used when the backend sets no `base_url` of its own.
/// Already versioned — the batch calls append their paths (`/models/{model}:...`,
/// `/{batch_id}`, ...) directly onto this. See [`GeminiBatch::base_url`] for how a
/// *configured* `base_url` (a HOST ROOT, like rig's own Gemini `ClientBuilder`) gets
/// versioned to match.
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
/// (surfaced per item, never silently dropped — the Anthropic per-item ethos). `submitted`
/// rides into the cross-check ([`cross_check_absentees`]) — the caller passes `None` when
/// `arr` is a known-partial result set (a cancelled/expired batch's before-the-cutoff
/// items), so a legitimate partial is never misreported as a silently missing item.
fn parse_gemini_inlined(arr: &[Value], submitted: Option<u64>) -> Vec<BatchAnswer> {
    let answers = arr
        .iter()
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
        .collect();
    cross_check_absentees(answers, submitted)
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
/// `submitted` (the count kaibo sent this batch with) reaches the cross-check ONLY on the
/// succeeded arm — that state is the one Gemini promises is the *whole* result set. The
/// cancelled/expired/failed arm below hands back whatever finished before the terminal
/// event, an outcome that is *expected* to be partial, so it always cross-checks against
/// `None`: a genuinely partial batch must never render its known-incomplete items as
/// kaibo's own "missing" absentees.
///
/// [`Failed`]: BatchPoll::Failed
fn parse_gemini_poll(v: &Value, submitted: Option<u64>) -> Result<BatchPoll> {
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
            Ok(BatchPoll::Done(parse_gemini_inlined(arr, submitted)))
        }
        "BATCH_STATE_CANCELLED" | "BATCH_STATE_FAILED" | "BATCH_STATE_EXPIRED" => {
            // Items that finished before the terminal event still carry results — hand
            // those back rather than throwing them away. A known partial, so no cross-check.
            if let Some(arr) = gemini_inlined_array(v) {
                if !arr.is_empty() {
                    return Ok(BatchPoll::Done(parse_gemini_inlined(arr, None)));
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

    /// The batch endpoint root: [`GEMINI_API_BASE`] (already `/v1beta`-versioned) when
    /// the backend sets no `base_url`; otherwise the configured base_url versioned
    /// with `/v1beta` here. The config contract is a HOST ROOT — matching rig's live
    /// Gemini `ClientBuilder` (which appends its own `/v1beta/models/...` path rather
    /// than taking one baked into `base_url`) and the anthropic kind's style — so a
    /// gateway/proxy address configured for the live path just works for batch too.
    /// A trailing slash, and a base_url someone pre-versioned with `/v1beta` (with or
    /// without a trailing slash), are both normalized first so a reasonable config
    /// never double-versions — the spirit of rig's `normalize_anthropic_base_url`.
    fn base_url(backend: &Backend) -> String {
        match backend.base_url.as_deref() {
            Some(configured) => {
                let trimmed = configured.trim_end_matches('/');
                let trimmed = trimmed.strip_suffix("/v1beta").unwrap_or(trimmed);
                format!("{trimmed}/v1beta")
            }
            None => GEMINI_API_BASE.to_string(),
        }
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
        // Record the resolved batch request shape (held for the submit's lifetime) — the
        // batch-lane analog of run_phase's gen_ai.request.thinking.
        let _span = batch_request_span(&self.model, self.params.as_ref(), items.len());
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

    async fn poll(&self, batch_id: &str, submitted: Option<u64>) -> Result<BatchPoll> {
        let url = format!("{}/{}", self.base_url, batch_id);
        let v = self.send_json(self.http.get(&url), "status").await?;
        parse_gemini_poll(&v, submitted)
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

// --- OpenAI provider -------------------------------------------------------

/// The endpoint every kaibo OpenAI batch item targets. Responses is also the *interactive*
/// hosted-GPT path (`consult/engine.rs` routes an exact-Platform backend through rig's
/// Responses client), so batch and interactive share one body shape, one reasoning knob
/// (`reasoning.effort`), and one answer parser. A Chat Completions lane would be a second
/// of each for no model kaibo can reach today; if one ever needs it, it's a new endpoint
/// constant plus its own body/answer pair, not a flag on this one.
const OPENAI_BATCH_ENDPOINT: &str = "/v1/responses";

/// The only completion window OpenAI's Batch API accepts.
const OPENAI_COMPLETION_WINDOW: &str = "24h";

/// The filename kaibo gives its uploaded input. OpenAI keys batch input on the file's
/// `purpose`, not its name, but the name is what a human sees in the Files dashboard —
/// so it says who made it.
const OPENAI_INPUT_FILENAME: &str = "kaibo-batch-input.jsonl";

/// Batch's request shape for a *hosted* OpenAI slot — the Responses-flavored sibling of
/// [`batch_shaping`]. It can't go through [`ModelShape`](crate::consult::ModelShape):
/// that classifier maps the whole `openai` kind to [`ThinkingStyle::None`] on purpose
/// (a local llama.cpp/Ollama server takes no reasoning params), and hosted-ness is a
/// property of the *backend*, not the kind. So this reuses the same model-aware classifier
/// the interactive hosted arm uses, at the batch-floored effort instead of the interactive
/// default.
///
/// `max_tokens` and the effort are floored exactly like [`batch_shaping`] — same
/// [`batch_effort`] rule, so the two lanes can't drift apart on which direction wins.
/// Sampling is left to the provider default — `top_p = None` — matching the other
/// providers' batch shape.
pub fn openai_batch_shaping(model: &str, tunables: &SlotTunables) -> (u64, Option<Value>) {
    let max_tokens = tunables.max_tokens.max(BATCH_MAX_TOKENS_FLOOR);
    let params =
        crate::consult::hosted_openai_responses_params(
            model,
            None,
            batch_effort(&tunables.effort),
            tunables.thinking_style,
        );
    (max_tokens, params)
}

/// Build one Responses `input` for a prompt plus the shared attachments. With no
/// attachments it stays a bare string (the Responses API's own shorthand, and the
/// unchanged wire shape). With attachments it becomes one user turn whose content is the
/// attachments first as context (a text file as an `input_text` under its `<file>` wrapper,
/// an image as an `input_image` data URL), then the prompt last.
fn openai_input(attachments: &[Attachment], prompt: &str) -> Value {
    if attachments.is_empty() {
        return json!(prompt);
    }
    let mut parts: Vec<Value> = attachments
        .iter()
        .map(|att| match att {
            Attachment::Text { .. } => json!({
                "type": "input_text",
                "text": att.wrapped_text().expect("text attachment wraps")
            }),
            Attachment::Image { mime, data_b64, .. } => json!({
                "type": "input_image",
                "image_url": format!("data:{mime};base64,{data_b64}")
            }),
        })
        .collect();
    parts.push(json!({ "type": "input_text", "text": prompt }));
    json!([{ "role": "user", "content": Value::Array(parts) }])
}

/// Build the JSONL input body — one line per item, each a self-contained Responses
/// request: `custom_id` (kaibo's item index, the correlation key that comes back in the
/// output file), the `POST` + `url` OpenAI's batch runner replays, and a `body` carrying
/// the model, the shared preamble as `instructions`, the item's input, the floored
/// `max_output_tokens`, and the flattened shaping params (`reasoning.effort`).
///
/// Pure, so the wire shape is pinned without a network — and note the whole file is a
/// `String` built in memory. kaibo writes no local file here: the "file" OpenAI's batch
/// API needs is a resource in the *caller's OpenAI account*, POSTed straight from this
/// buffer (see [`OpenaiBatch::upload_input`]).
pub fn openai_batch_jsonl(
    model: &str,
    max_tokens: u64,
    system: &str,
    params: &Option<Value>,
    attachments: &[Attachment],
    items: &[BatchItem],
) -> Result<String> {
    let mut out = String::new();
    for it in items {
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        if !system.is_empty() {
            body.insert("instructions".into(), json!(system));
        }
        body.insert("input".into(), openai_input(attachments, &it.prompt));
        body.insert("max_output_tokens".into(), json!(max_tokens));
        if let Some(Value::Object(extra)) = params {
            for (k, v) in extra {
                body.insert(k.clone(), v.clone());
            }
        }
        let line = json!({
            "custom_id": it.custom_id,
            "method": "POST",
            "url": OPENAI_BATCH_ENDPOINT,
            "body": Value::Object(body),
        });
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

/// Where an OpenAI batch object says it is. Mirrors Anthropic's [`StatusKind`]: the
/// terminal arm carries the *file ids* rather than results, because OpenAI hands results
/// back as account files the poll then downloads.
#[derive(Debug, PartialEq, Eq)]
enum OpenaiStatus {
    Pending {
        completed: u64,
        total: u64,
    },
    Cancelling,
    /// A terminal state (`completed`/`failed`/`expired`/`cancelled`). Either file id may
    /// be absent: a validation `failed` batch has neither, while `expired`/`cancelled`
    /// can still carry the items that finished before the clock or the cancel caught up.
    Ended {
        state: String,
        output_file_id: Option<String>,
        error_file_id: Option<String>,
    },
}

/// Read an OpenAI batch's `request_counts` into `(completed, total)`. `completed` sums the
/// terminal buckets (`completed` + `failed`); `total` is the provider's own `total`. A
/// missing block reads as 0/0 rather than erroring — a just-created batch has no counts.
fn openai_counts(v: &Value) -> (u64, u64) {
    let c = v.get("request_counts");
    let get = |k: &str| {
        c.and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    (get("completed") + get("failed"), get("total"))
}

/// Read a batch object's `status` (+ counts, + result file ids) into an [`OpenaiStatus`].
/// An unknown status is a loud error, not a guess — OpenAI's set is closed and a new
/// member is something we want to see, not silently mis-render as pending.
fn parse_openai_status(v: &Value) -> Result<OpenaiStatus> {
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch object has no `status`: {v}"))?;
    let file_id = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match status {
        "validating" | "in_progress" | "finalizing" => {
            let (completed, total) = openai_counts(v);
            Ok(OpenaiStatus::Pending { completed, total })
        }
        "cancelling" => Ok(OpenaiStatus::Cancelling),
        "completed" | "failed" | "expired" | "cancelled" => Ok(OpenaiStatus::Ended {
            state: status.to_string(),
            output_file_id: file_id("output_file_id"),
            error_file_id: file_id("error_file_id"),
        }),
        other => Err(anyhow!("unknown batch status {other:?}")),
    }
}

/// The batch object's own `errors` — input-file validation failures, which is why a
/// `failed` batch has no results at all. Joined into one readable reason (with the
/// offending JSONL line number where the provider gives one). `None` when the provider
/// offered no detail, so the caller can say *that* rather than print an empty reason.
fn openai_batch_error_text(v: &Value) -> Option<String> {
    let joined = v
        .get("errors")
        .and_then(|e| e.get("data"))
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|e| {
            let msg = e.get("message").and_then(Value::as_str)?;
            Some(match e.get("line").and_then(Value::as_u64) {
                Some(line) => format!("line {line}: {msg}"),
                None => msg.to_string(),
            })
        })
        .collect::<Vec<_>>()
        .join("; ");
    (!joined.is_empty()).then_some(joined)
}

/// The answer text of a Responses body: the assistant message's `output_text` parts,
/// joined. The `output` array also carries `reasoning` items — the model's scratchpad,
/// not the answer — so they're skipped, the way the Gemini path skips `thought` parts.
fn openai_response_text(body: &Value) -> String {
    body.get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
                .filter_map(|i| i.get("content").and_then(Value::as_array))
                .flatten()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("output_text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Gate a Responses body's own `status`. `completed` is a clean finish; `incomplete`
/// names why it stopped (`max_output_tokens` is the truncation case max thinking makes
/// common — the GH #75 shape); `failed` carries the model-run error. An absent status is
/// treated as clean, with the empty-text check still catching a genuinely empty answer.
fn openai_finish_gate(body: &Value) -> Result<(), String> {
    match body.get("status").and_then(Value::as_str) {
        Some("completed") | None => Ok(()),
        Some("incomplete") => {
            let reason = body
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let note = match reason {
                "max_output_tokens" => {
                    "the response hit its output-token budget before finishing — common \
                     when a big attached-file review spends the budget thinking; re-run \
                     with a higher max_tokens or a narrower prompt"
                }
                "content_filter" => "the provider halted generation on a content policy",
                _ => "the response did not complete normally",
            };
            Err(format!("incomplete ({reason}): {note}"))
        }
        Some("failed") => {
            let detail = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            Err(format!("the model run failed: {detail}"))
        }
        Some(other) => Err(format!(
            "status={other}: the response did not complete normally"
        )),
    }
}

/// Parse an output- or error-file body: one `{custom_id, response, error}` per line. The
/// *same* parser reads both files — OpenAI splits succeeded and failed requests across
/// them, but the line shape is identical, and a per-item failure is surfaced as an
/// `Err(reason)` either way rather than silently dropped.
fn parse_openai_output_jsonl(body: &str) -> Result<Vec<BatchAnswer>> {
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
        // A non-null `error` is the request never reaching the model at all.
        let request_error = v.get("error").filter(|e| !e.is_null());
        let response = v.get("response").filter(|r| !r.is_null());
        let text = match (request_error, response) {
            (Some(err), _) => {
                let detail = err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| err.get("code").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| err.to_string());
                Err(format!("provider error: {detail}"))
            }
            (None, Some(resp)) => {
                let code = resp
                    .get("status_code")
                    .and_then(Value::as_u64)
                    .unwrap_or(200);
                let body = resp.get("body");
                if code != 200 {
                    // A non-2xx per-item response carries an OpenAI error envelope.
                    let detail = body
                        .and_then(|b| b.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| body.map(|b| b.to_string()))
                        .unwrap_or_else(|| "no detail given".into());
                    Err(format!("provider error (HTTP {code}): {detail}"))
                } else {
                    let body = body.ok_or_else(|| {
                        anyhow!("succeeded result has no `response.body`: {line}")
                    })?;
                    finish_gated_answer(openai_response_text(body), openai_finish_gate(body))
                }
            }
            (None, None) => Err(
                "the provider returned neither a response nor an error for this item".to_string(),
            ),
        };
        out.push(BatchAnswer { custom_id, text });
    }
    Ok(out)
}

/// Put a merged result set back in submit order. kaibo's `custom_id`s are item indices, but
/// OpenAI hands succeeded and failed items back in *separate files*, so the merge order is
/// an artifact of which file they landed in — sorting by index restores the caller's own
/// ordering. Only when every id is numeric: a future non-index `custom_id` keeps the
/// provider's order untouched rather than being silently reshuffled.
fn order_by_index(mut answers: Vec<BatchAnswer>) -> Vec<BatchAnswer> {
    if answers.iter().all(|a| a.custom_id.parse::<u64>().is_ok()) {
        answers.sort_by_key(|a| a.custom_id.parse::<u64>().unwrap_or(0));
    }
    answers
}

/// Cross-check + reorder an OpenAI batch's merged output+error answers — the one place
/// the check can run, since a per-file parse alone can't tell a genuinely missing item
/// from one sitting in the *other* file. Only a `completed` batch promises the two files
/// together cover every submitted item; `expired`/`cancelled` legitimately hand back a
/// partial set (whatever beat the clock/cancel), so `submitted` is honored only for
/// `state == "completed"` — any other terminal state cross-checks against `None`, so a
/// known partial is never misreported as kaibo's own "missing" absentee.
fn finalize_openai_answers(
    answers: Vec<BatchAnswer>,
    state: &str,
    submitted: Option<u64>,
) -> Vec<BatchAnswer> {
    let expected = (state == "completed").then_some(submitted).flatten();
    order_by_index(cross_check_absentees(answers, expected))
}

/// Parse one batch object from the list endpoint. A missing `id`/`status` is a broken
/// contract, surfaced rather than papered over; counts are best-effort. `created_at` is
/// Unix epoch seconds on this provider (Anthropic and Gemini send RFC3339 strings), so it's
/// rendered to RFC3339 here — [`BatchListItem::created_at`] is the shared string field the
/// recency filter reads back through [`rfc3339_to_epoch`].
fn parse_openai_list_item(v: &Value) -> Result<BatchListItem> {
    let provider_id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch list item has no `id`: {v}"))?
        .to_string();
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("batch list item has no `status`: {v}"))?
        .to_string();
    let (completed, total) = openai_counts(v);
    let created_at = v
        .get("created_at")
        .and_then(Value::as_i64)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339());
    Ok(BatchListItem {
        provider_id,
        status,
        completed,
        total,
        created_at,
    })
}

/// Parse the list endpoint's `{ data: [...], has_more }` page (newest first), carrying
/// `has_more` back so a truncated view announces itself.
fn parse_openai_list(v: &Value) -> Result<(Vec<BatchListItem>, bool)> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("batch list response has no `data` array: {v}"))?;
    let items = data
        .iter()
        .map(parse_openai_list_item)
        .collect::<Result<Vec<_>>>()?;
    let has_more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
    Ok((items, has_more))
}

/// OpenAI Batch over HTTP — the *file-based* protocol, and the odd one out. Anthropic
/// carries its requests inline in one POST and Gemini nests them inline too; OpenAI takes
/// them as a file resource in the caller's own OpenAI account (upload JSONL → create a
/// batch against it → poll → download the result files).
///
/// **No local file, ever.** The JSONL is built in memory and streamed straight into the
/// multipart upload; results come back as strings. kaibo's read-only invariant is untouched
/// by this lane — the only artifacts live in the user's OpenAI account.
///
/// **kaibo deletes nothing.** The input file is, in fact, the only surviving record of what
/// was submitted (kaibo's store keeps the handle and label, never the prompts), and deleting
/// an output file on poll would break re-polling a finished handle. OpenAI expires batch
/// output files on its own (30 days today). An opt-in cleanup flag is tracked in
/// `docs/issues.md`.
pub struct OpenaiBatch {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Submit-only: empty on a poll/cancel-only client.
    model: String,
    max_tokens: u64,
    params: Option<Value>,
}

impl OpenaiBatch {
    /// Refuse a backend this provider can't serve. Two gates, because `openai` is the one
    /// *kind* whose batch capability is a per-backend property: the wrong kind is a
    /// dispatch bug (belt-and-suspenders, like the other providers), while a generic
    /// OpenAI-compatible endpoint is an ordinary user misconfiguration that deserves a
    /// message naming the fix.
    fn require_hosted(backend: &Backend) -> Result<()> {
        if backend.kind != ProviderKind::Openai {
            return Err(anyhow!(
                "this is the OpenAI batch provider, but backend {:?} is {:?}. (Batch \
                 dispatch should never route a non-OpenAI backend here; see \
                 `submitter`/`poller`.)",
                backend.name,
                backend.kind
            ));
        }
        if !backend.is_hosted_openai() {
            return Err(anyhow!(
                "backend {:?} is an OpenAI-compatible endpoint at {}, not OpenAI Platform — \
                 the Batch API is Platform's, so a local server or gateway has no \
                 /v1/batches to submit to. A batch cast's synth needs a backend whose \
                 base_url is exactly {}.",
                backend.name,
                backend.resolved_base_url(),
                crate::credentials::HOSTED_OPENAI_BASE_URL
            ));
        }
        Ok(())
    }

    /// The API root, trailing slash trimmed so `{base}/files` never doubles it.
    fn base_url(backend: &Backend) -> String {
        backend
            .resolved_base_url()
            .trim_end_matches('/')
            .to_string()
    }

    /// A submit-capable client: the cast's synth slot, shaped to batch's maxed knobs.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot, defaults: &Defaults) -> Result<Self> {
        Self::require_hosted(backend)?;
        let tunables = slot.tunables(ModelRole::Synth, defaults);
        let (max_tokens, params) = openai_batch_shaping(&slot.id, &tunables);
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
        Self::require_hosted(backend)?;
        Ok(Self {
            http: batch_http_client(backend)?,
            api_key: backend.resolve_key()?,
            base_url: Self::base_url(backend),
            model: String::new(),
            max_tokens: 0,
            params: None,
        })
    }

    /// Send a request under the bearer key, returning the parsed JSON body or a loud error
    /// carrying the raw text.
    async fn send_json(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let text = self.send_text(req, what).await?;
        serde_json::from_str(&text).with_context(|| format!("parsing openai batch {what}: {text}"))
    }

    /// The same call, returning the raw body — the result files are JSONL, not JSON.
    async fn send_text(&self, req: reqwest::RequestBuilder, what: &str) -> Result<String> {
        let resp = req
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| anyhow!("openai batch {what} failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading openai batch {what} body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("openai batch {what} {status}: {text}"));
        }
        Ok(text)
    }

    /// Upload the in-memory JSONL as a `purpose = "batch"` file, returning its id. The
    /// bytes go straight from the buffer into the multipart body — nothing is written
    /// locally (see the type doc).
    async fn upload_input(&self, jsonl: String) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(jsonl.into_bytes())
            .file_name(OPENAI_INPUT_FILENAME)
            .mime_str("application/jsonl")
            .map_err(|e| anyhow!("building openai batch upload part: {e}"))?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", part);
        let url = format!("{}/files", self.base_url);
        let v = self
            .send_json(self.http.post(&url).multipart(form), "input upload")
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("uploaded batch input has no string `id`: {v}"))
    }

    /// Download a result file's JSONL body.
    async fn fetch_file_text(&self, file_id: &str, what: &str) -> Result<String> {
        let url = format!("{}/files/{file_id}/content", self.base_url);
        self.send_text(self.http.get(&url), what).await
    }
}

#[async_trait]
impl BatchProvider for OpenaiBatch {
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
        // Record the resolved batch request shape (held for the submit's lifetime) — the
        // batch-lane analog of run_phase's gen_ai.request.thinking.
        let _span = batch_request_span(&self.model, self.params.as_ref(), items.len());
        let jsonl = openai_batch_jsonl(
            &self.model,
            self.max_tokens,
            system,
            &self.params,
            attachments,
            items,
        )?;
        let input_file_id = self.upload_input(jsonl).await?;
        let body = json!({
            "input_file_id": input_file_id,
            "endpoint": OPENAI_BATCH_ENDPOINT,
            "completion_window": OPENAI_COMPLETION_WINDOW,
        });
        let url = format!("{}/batches", self.base_url);
        let v = self
            .send_json(
                self.http
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(serde_json::to_vec(&body)?),
                "submit",
            )
            .await?;
        parse_batch_id(&v)
    }

    async fn poll(&self, batch_id: &str, submitted: Option<u64>) -> Result<BatchPoll> {
        let url = format!("{}/batches/{batch_id}", self.base_url);
        let v = self.send_json(self.http.get(&url), "status").await?;
        match parse_openai_status(&v)? {
            OpenaiStatus::Pending { completed, total } => {
                Ok(BatchPoll::Pending { completed, total })
            }
            OpenaiStatus::Cancelling => Ok(BatchPoll::Cancelling),
            OpenaiStatus::Ended {
                state,
                output_file_id,
                error_file_id,
            } => {
                // Succeeded and failed items live in *separate* files; read both so a
                // per-item failure is reported rather than quietly missing from the set.
                let mut answers = Vec::new();
                if let Some(id) = output_file_id {
                    answers.extend(parse_openai_output_jsonl(
                        &self.fetch_file_text(&id, "results").await?,
                    )?);
                }
                if let Some(id) = error_file_id {
                    answers.extend(parse_openai_output_jsonl(
                        &self.fetch_file_text(&id, "errors").await?,
                    )?);
                }
                if answers.is_empty() {
                    // A validation `failed` batch never ran an item; the reason is on the
                    // batch object itself. Name it instead of rendering "0 results".
                    return Ok(BatchPoll::Failed {
                        state,
                        message: openai_batch_error_text(&v).unwrap_or_else(|| {
                            "the provider returned no result file and no error detail".to_string()
                        }),
                    });
                }
                Ok(BatchPoll::Done(finalize_openai_answers(
                    answers, &state, submitted,
                )))
            }
        }
    }

    async fn cancel(&self, batch_id: &str) -> Result<()> {
        let url = format!("{}/batches/{batch_id}/cancel", self.base_url);
        self.send_json(self.http.post(&url), "cancel").await?;
        Ok(())
    }

    async fn list(&self) -> Result<(Vec<BatchListItem>, bool)> {
        // One page at the API's max (100), newest-first by default. `has_more` rides back
        // so a truncated view announces itself — orphan recovery wants the recent tail.
        let url = format!("{}/batches?limit=100", self.base_url);
        let v = self.send_json(self.http.get(&url), "list").await?;
        parse_openai_list(&v)
    }
}

// --- Dispatch --------------------------------------------------------------

/// Does this *backend* have a batch lane? The single source of truth — dispatch
/// ([`submitter`]/[`poller`]), `list`'s backend filter, cast validation, and refusal
/// messages all defer to it, so adding a provider is one arm here.
///
/// Keyed on the backend, not the kind, because `openai` is the kind whose batch capability
/// is a per-*backend* property: OpenAI's Batch API belongs to OpenAI Platform, so an
/// `openai`-kind backend aimed at a local llama.cpp/Ollama server or a compatibility
/// gateway has no `/v1/batches` to submit to. Same seam the interactive Responses routing
/// uses ([`Backend::is_hosted_openai`]).
pub fn batch_supported(backend: &Backend) -> bool {
    // A media kind is not a completion wire at all — no batch endpoint, and no
    // reasoning slot can point at one (see `ProviderKind::class`).
    let Some(wire) = backend.kind.wire() else {
        return false;
    };
    match wire {
        WireKind::Anthropic | WireKind::Gemini => true,
        WireKind::Openai => backend.is_hosted_openai(),
        WireKind::DeepSeek | WireKind::OpenRouter => false,
    }
}

/// The batch-capable kinds as a display list, naming `openai`'s condition — that kind's
/// lane is per-backend, so a bare "openai" would send a user with a local server in
/// circles (see [`batch_supported`]).
pub fn supported_kinds_list() -> String {
    "anthropic, gemini, openai (hosted OpenAI Platform only — not a local \
     OpenAI-compatible server)"
        .to_string()
}

/// The refusal for a backend with no batch lane — shared by [`submitter`] and [`poller`] so
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
        ProviderKind::Openai if backend.is_hosted_openai() => {
            Ok(Arc::new(OpenaiBatch::from_slot(backend, slot, defaults)?))
        }
        _ => Err(unsupported(backend)),
    }
}

/// Build a poll/cancel/list-only provider from a resolved backend, dispatching on kind.
pub fn poller(backend: &Backend) -> Result<Arc<dyn BatchProvider>> {
    match backend.kind {
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicBatch::from_backend(backend)?)),
        ProviderKind::Gemini => Ok(Arc::new(GeminiBatch::from_backend(backend)?)),
        ProviderKind::Openai if backend.is_hosted_openai() => {
            Ok(Arc::new(OpenaiBatch::from_backend(backend)?))
        }
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

        async fn poll(&self, _batch_id: &str, _submitted: Option<u64>) -> Result<BatchPoll> {
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
            wire: None,
        }
    }

    fn tunables(effort: &str, max_tokens: u64) -> SlotTunables {
        SlotTunables {
            max_tokens,
            thinking_budget: 1024,
            temperature: 1.0,
            top_p: 1.0,
            effort: effort.to_string(),
            effort_explicit: true,
            thinking_style: crate::consult::ThinkingStyleOverride::Auto,
        }
    }

    /// Batch floors the knobs: a slot tuned thin for interactive use (tiny max_tokens,
    /// `low` effort) is lifted to the batch budget and the batch effort. The adaptive
    /// tier expresses that as `output_config.effort: "high"`. This is the direction that
    /// was always the intent — a cast tuned down for flaky interactive use still batches
    /// hot — and it must survive effort becoming a floor rather than an override.
    #[test]
    fn shaping_floors_tokens_and_lifts_thin_effort_adaptive() {
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
            "a shallower slot effort must be lifted to the batch floor: {params}"
        );
    }

    /// The other two directions of the effort floor, which the old force-in-both-directions
    /// shaping got wrong: a slot asking **deeper** than `BATCH_EFFORT` keeps its value (the
    /// same never-cap-a-richer-slot promise `max_tokens` and the thinking budget already
    /// make — a cast pinned to `xhigh` for the offline lane was being silently demoted),
    /// and a rung kaibo can't rank defers to the operator (effort is a passthrough string;
    /// quietly replacing an unrecognized rung with "high" is the override this retires).
    #[test]
    fn shaping_effort_floor_keeps_a_deeper_slot_and_defers_to_an_unranked_rung() {
        let (_max, params) = batch_shaping(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            &tunables("xhigh", 100),
        );
        let params = params.expect("an adaptive Anthropic model carries a thinking block");
        assert_eq!(
            params["output_config"]["effort"], "xhigh",
            "a slot deeper than the batch floor must keep its effort: {params}"
        );

        // A rung outside EFFORT_LADDER — a provider's newer name, or a typo; from here
        // they look identical, and trusting the operator is what keeps passthrough real.
        let (_max, params) = batch_shaping(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            &tunables("ludicrous", 100),
        );
        let params = params.expect("an adaptive Anthropic model carries a thinking block");
        assert_eq!(
            params["output_config"]["effort"], "ludicrous",
            "an unrankable rung passes through untouched: {params}"
        );
    }

    /// The reasoning **off-switch** survives the batch depth floor. `none` is not the
    /// shallowest rung — it is a different kind of answer — and a floor that raises
    /// *depth* has no business switching reasoning back on.
    ///
    /// It used to. `EFFORT_LADDER` carried `"none"` at index 0, so `batch_effort` ranked
    /// it below `BATCH_EFFORT` and lifted it to `"high"`, quietly billing reasoning on
    /// every item of a fan-out the operator had turned it off for — and a fan-out is
    /// exactly where a bulk-extraction batch turns reasoning off, and exactly where the
    /// per-item cost multiplies. Two reviewers read the same code and reached opposite
    /// conclusions about this, because `effort_rank` and `is_effort_off` each acted as
    /// though the other didn't exist.
    #[test]
    fn batch_effort_floor_leaves_the_reasoning_off_switch_alone() {
        assert_eq!(
            batch_effort(crate::consult::EFFORT_OFF),
            crate::consult::EFFORT_OFF,
            "a DEPTH floor must not turn reasoning on — off is not a shallow depth"
        );
        assert_eq!(batch_effort("NONE"), "NONE", "however it was spelled");
        assert_eq!(batch_effort(" none "), " none ", "however it was spaced");
        assert!(
            crate::consult::effort_rank(crate::consult::EFFORT_OFF).is_none(),
            "the off-switch must not be rankable — ranking it is what let a floor lift it"
        );

        // And it reaches the wire as the structural disable, not as a lifted depth: the
        // shaping half of the same contract.
        let (_max, params) = batch_shaping(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            &tunables("none", 100),
        );
        let params = params.expect("deepseek carries a thinking block");
        assert_eq!(
            params["thinking"]["type"], "disabled",
            "a batch item with reasoning off must actually run with it off: {params}"
        );
        assert!(params.get("reasoning_effort").is_none(), "{params}");
    }

    /// `batch_effort`'s floor only exists if `BATCH_EFFORT` is rankable. Set it to
    /// anything absent from `EFFORT_LADDER` — a typo, a provider's newer rung, or the
    /// off-switch — and the wildcard arm returns the slot's value for *every* input,
    /// silently disabling the floor with no test noticing. This is the guard: the
    /// constant has to be a depth kaibo can order, and it must not be the off-switch.
    #[test]
    fn batch_effort_floor_constant_is_a_rankable_depth() {
        assert!(
            crate::consult::effort_rank(BATCH_EFFORT).is_some(),
            "BATCH_EFFORT {BATCH_EFFORT:?} is not on EFFORT_LADDER {:?} — the batch floor \
             would silently do nothing at all",
            crate::consult::EFFORT_LADDER
        );
        assert!(
            !crate::consult::is_effort_off(BATCH_EFFORT),
            "BATCH_EFFORT is a floor on depth; the off-switch is not a depth"
        );
    }

    /// The floor rule itself, isolated from any provider shape — every direction in one
    /// place so a future edit to `batch_effort` can't quietly flip one of them.
    #[test]
    fn batch_effort_is_a_floor_not_an_override() {
        assert_eq!(batch_effort("minimal"), BATCH_EFFORT, "lifted");
        assert_eq!(batch_effort("low"), BATCH_EFFORT, "lifted");
        assert_eq!(
            batch_effort(BATCH_EFFORT),
            BATCH_EFFORT,
            "equal stays equal"
        );
        assert_eq!(batch_effort("xhigh"), "xhigh", "deeper wins");
        assert_eq!(batch_effort("max"), "max", "deeper wins");
        assert_eq!(batch_effort("ludicrous"), "ludicrous", "unranked defers");
        // The off-switch is not a rung and is not lifted — its own test above owns why.
        assert_eq!(batch_effort("none"), "none", "off stays off");
        // Ranking is case/whitespace tolerant, but the slot's own spelling is what ships —
        // kaibo never normalizes an operator's string on its way to a provider.
        assert_eq!(
            batch_effort(" LOW "),
            BATCH_EFFORT,
            "ranked case-insensitively"
        );
        assert_eq!(batch_effort("XHIGH"), "XHIGH", "deeper keeps its spelling");
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
        let answers = parse_results_jsonl(body, None).unwrap();
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

    /// `docs/issues.md`'s P3 batch cross-check, Anthropic side: a provider that silently
    /// drops an item yields fewer lines than kaibo submitted, with no error naming the
    /// gap. Submitting 3 but reading back 2 must synthesize the third as a loud per-item
    /// failure — never a quiet "2 results" that reads like the batch only ever had two.
    #[test]
    fn results_jsonl_cross_checks_submitted_ids() {
        let body = concat!(
            r#"{"custom_id":"0","result":{"type":"succeeded","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}}"#,
            "\n",
            r#"{"custom_id":"1","result":{"type":"errored","error":{"type":"overloaded"}}}"#,
            "\n",
            // custom_id "2" never came back.
        );
        let answers = parse_results_jsonl(body, Some(3)).unwrap();
        assert_eq!(
            answers.len(),
            3,
            "the gap is filled, not left short: {answers:?}"
        );
        let absentee = &answers[2];
        assert_eq!(absentee.custom_id, "2");
        let msg = absentee
            .text
            .as_ref()
            .expect_err("missing item is a failure");
        assert!(
            msg.contains("kaibo"),
            "names itself, not the provider: {msg}"
        );
        assert!(
            !msg.contains("provider error:"),
            "must not read like a provider-reported failure: {msg}"
        );
        assert!(msg.contains("dashboard"), "names what to do: {msg}");

        // No submitted count on the handle (a legacy record, or persistence off) — the
        // gap stays silent rather than inventing a count to check against.
        let answers = parse_results_jsonl(body, None).unwrap();
        assert_eq!(
            answers.len(),
            2,
            "with no submitted count, the result set renders exactly as it always did"
        );
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
        let answers = parse_results_jsonl(body, None).unwrap();
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
        let answers = parse_results_jsonl(body, None).unwrap();
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

    /// A backend's configured `base_url` (an Anthropic-Messages-API-compatible
    /// gateway/proxy) must actually reach the batch client — the bug a cross-family
    /// review caught: `AnthropicBatch` once hardcoded `ANTHROPIC_API_BASE` and
    /// silently ignored `backend.base_url`, so interactive `consult` reached a custom
    /// gateway while batch calls kept dialing api.anthropic.com. Both constructors
    /// must resolve it, and an unset `base_url` must still fall back to the default.
    #[test]
    fn base_url_reaches_the_batch_client() {
        let mut b = anthropic_backend();
        b.base_url = Some("https://gateway.example.ts.net".to_string());
        b.key_optional = true;
        let slot = ModelSlot::bare(&b.name, "claude-sonnet-4-6");

        let submit_client = AnthropicBatch::from_slot(&b, &slot, &Defaults::default()).unwrap();
        assert_eq!(submit_client.base_url, "https://gateway.example.ts.net");

        let poll_client = AnthropicBatch::from_backend(&b).unwrap();
        assert_eq!(poll_client.base_url, "https://gateway.example.ts.net");

        let mut default_backend = anthropic_backend();
        default_backend.key_optional = true;
        let default_client = AnthropicBatch::from_backend(&default_backend).unwrap();
        assert_eq!(default_client.base_url, ANTHROPIC_API_BASE);
    }

    /// The Gemini batch lane's base URL contract: a configured `backend.base_url` is
    /// a HOST ROOT (mirroring rig's live Gemini `ClientBuilder`, which appends its own
    /// `/v1beta/...` path — see the engine-arm test of the same shape), so
    /// `GeminiBatch::base_url` must version it with `/v1beta` itself rather than use it
    /// verbatim, unlike `AnthropicBatch::base_url` (whose `/v1/messages/batches` suffix
    /// is appended at call time, not baked into the stored base). A trailing slash is
    /// trimmed, and a base_url the user already versioned (with or without a trailing
    /// slash) is not double-versioned — the spirit of rig's own
    /// `normalize_anthropic_base_url`. The unset fallback is untouched.
    #[test]
    fn gemini_batch_base_url_versions_a_configured_host_root() {
        let mut b = anthropic_backend();
        b.kind = ProviderKind::Gemini;
        b.key_optional = true;

        b.base_url = Some("https://llm-gateway.example.internal".to_string());
        assert_eq!(
            GeminiBatch::base_url(&b),
            "https://llm-gateway.example.internal/v1beta"
        );

        // Trailing slash on the configured host root.
        b.base_url = Some("https://llm-gateway.example.internal/".to_string());
        assert_eq!(
            GeminiBatch::base_url(&b),
            "https://llm-gateway.example.internal/v1beta"
        );

        // Already pre-versioned — must not become .../v1beta/v1beta.
        b.base_url = Some("https://llm-gateway.example.internal/v1beta".to_string());
        assert_eq!(
            GeminiBatch::base_url(&b),
            "https://llm-gateway.example.internal/v1beta"
        );

        // Pre-versioned with a trailing slash too.
        b.base_url = Some("https://llm-gateway.example.internal/v1beta/".to_string());
        assert_eq!(
            GeminiBatch::base_url(&b),
            "https://llm-gateway.example.internal/v1beta"
        );

        // Unset still falls back to the default, unversioned by this logic (it's
        // already baked into GEMINI_API_BASE).
        b.base_url = None;
        assert_eq!(GeminiBatch::base_url(&b), GEMINI_API_BASE);
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

    /// A 3-line id takes `thinkingLevel` — the per-role effort lever, not a token budget —
    /// lifted to BATCH_EFFORT when the slot asked for a shallower "low". (The
    /// `gemini-batch` cast synths `gemini-pro-latest`, also a level-tier id now that the
    /// whole Gemini 3-line takes a level; every Gemini id runs this one path through
    /// `batch_shaping`.) Note the ladder above `high` is Google's to answer for on this
    /// wire: batch POSTs its own JSON (no rig converter in the path), so a slot pinned
    /// deeper than `high` reaches Google verbatim and Google decides — the passthrough
    /// contract, applied honestly rather than quietly clamped back to `high`.
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

    /// The `batch_submit` span makes the batch lane wire-legible like `run_phase`: it must
    /// record `model` and the resolved `gen_ai.request.thinking` blob (here a Gemini
    /// `thinkingLevel` at `BATCH_EFFORT`), and record *no* thinking field for a toggle-less
    /// shape (`params == None`). Discriminating on both edges, so a regression that drops
    /// the field or records it unconditionally fails one branch. `batch_request_span` owns
    /// both the field declaration and the record, so this covers the exact production
    /// span-builder without a live submit.
    #[test]
    fn batch_submit_span_carries_the_thinking_params() {
        use crate::test_support::serialized_capture;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        fn dbg_str(v: &dyn std::fmt::Debug) -> String {
            format!("{v:?}")
        }
        #[derive(Default)]
        struct Grab {
            model: Option<String>,
            thinking: Option<String>,
        }
        impl tracing::field::Visit for Grab {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                match f.name() {
                    "model" => self.model = Some(dbg_str(v)),
                    "gen_ai.request.thinking" => self.thinking = Some(dbg_str(v)),
                    _ => {}
                }
            }
        }
        #[derive(Default)]
        struct Stored {
            model: Option<String>,
            thinking: Option<String>,
        }
        /// One closed `batch_submit` span's `(model, thinking)`.
        type Captured = (Option<String>, Option<String>);
        /// Collects each closed `batch_submit` span's fields.
        #[derive(Clone, Default)]
        struct Cap(Arc<Mutex<Vec<Captured>>>);
        impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Cap {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                id: &tracing::Id,
                ctx: Context<'_, S>,
            ) {
                if attrs.metadata().name() != "batch_submit" {
                    return;
                }
                let mut g = Grab::default(); // model lands here (%model at creation)
                attrs.record(&mut g);
                ctx.span(id).unwrap().extensions_mut().insert(Stored {
                    model: g.model,
                    thinking: g.thinking, // Empty at open — the record fills it below.
                });
            }
            fn on_record(
                &self,
                id: &tracing::Id,
                values: &tracing::span::Record<'_>,
                ctx: Context<'_, S>,
            ) {
                let mut g = Grab::default();
                values.record(&mut g);
                if let (Some(t), Some(span)) = (g.thinking, ctx.span(id)) {
                    if let Some(st) = span.extensions_mut().get_mut::<Stored>() {
                        st.thinking = Some(t);
                    }
                }
            }
            fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
                let span = ctx.span(&id).unwrap();
                if span.name() != "batch_submit" {
                    return;
                }
                let (m, t) = span
                    .extensions()
                    .get::<Stored>()
                    .map(|s| (s.model.clone(), s.thinking.clone()))
                    .unwrap_or_default();
                self.0.lock().unwrap().push((m, t));
            }
        }

        // The on-theme Gemini batch shape: thinkingLevel forced to BATCH_EFFORT.
        let (_max, params) = batch_shaping(
            ProviderKind::Gemini,
            "gemini-pro-latest",
            &tunables("low", 100),
        );
        let expected = params.expect("gemini carries a thinking block").to_string();
        assert!(
            expected.contains("thinkingLevel"),
            "fixture must be the level blob, got {expected}"
        );

        let cap = Cap::default();
        serialized_capture(async {
            let sub = tracing_subscriber::registry().with(cap.clone());
            let _g = tracing::subscriber::set_default(sub);
            // Positive: a real Gemini shape → model + thinking recorded.
            let (_m, params) = batch_shaping(
                ProviderKind::Gemini,
                "gemini-pro-latest",
                &tunables("low", 100),
            );
            drop(batch_request_span("gemini-pro-latest", params.as_ref(), 3));
            // Negative: no thinking block → the field stays absent.
            drop(batch_request_span("poll-only", None, 1));
        });

        let seen = cap.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "both spans closed, got {seen:?}");
        let pos = seen
            .iter()
            .find(|(m, _)| m.as_deref() == Some("gemini-pro-latest"))
            .expect("the gemini submit span");
        assert_eq!(
            pos.1.as_deref(),
            Some(expected.as_str()),
            "the batch_submit span must record the exact thinking blob"
        );
        let neg = seen
            .iter()
            .find(|(m, _)| m.as_deref() == Some("poll-only"))
            .expect("the toggle-less span");
        assert!(
            neg.1.is_none(),
            "a toggle-less shape records no thinking blob, got {:?}",
            neg.1
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
            parse_gemini_poll(&pend, None).unwrap(),
            BatchPoll::Pending {
                completed: 0,
                total: 2
            }
        );
        let run = json!({ "metadata": { "state": "BATCH_STATE_RUNNING",
            "batchStats": { "requestCount": "2", "successfulRequestCount": "1" } } });
        assert_eq!(
            parse_gemini_poll(&run, None).unwrap(),
            BatchPoll::Pending {
                completed: 1,
                total: 2
            }
        );
        // No state, or an unknown one, is a broken contract surfaced loudly.
        assert!(parse_gemini_poll(&json!({ "metadata": {} }), None).is_err());
        assert!(parse_gemini_poll(&json!({ "metadata": { "state": "WAT" } }), None).is_err());
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
        let poll = parse_gemini_poll(&v, None).unwrap();
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

    /// `docs/issues.md`'s P3 batch cross-check, Gemini side: a `SUCCEEDED` batch promises
    /// every submitted item is in the inlined array, so a provider that silently drops one
    /// must be caught here — submitting 3 but reading back 2 synthesizes the third as a
    /// loud per-item failure.
    #[test]
    fn gemini_poll_succeeded_cross_checks_submitted_ids() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_SUCCEEDED" },
            "response": { "inlinedResponses": { "inlinedResponses": [
                { "metadata": { "key": "0" }, "response": { "candidates": [ {
                    "finishReason": "STOP",
                    "content": { "parts": [ { "text": "answer" } ] } } ] } }
                // custom_id "1" never came back.
            ] } }
        });
        let answers = match parse_gemini_poll(&v, Some(2)).unwrap() {
            BatchPoll::Done(a) => a,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(answers.len(), 2, "the gap is filled: {answers:?}");
        let absentee = &answers[1];
        assert_eq!(absentee.custom_id, "1");
        let msg = absentee
            .text
            .as_ref()
            .expect_err("missing item is a failure");
        assert!(
            msg.contains("kaibo"),
            "names itself, not the provider: {msg}"
        );
        assert!(
            !msg.contains("provider error:"),
            "must not read like a provider-reported failure: {msg}"
        );
    }

    /// A cancelled/expired partial result set is a KNOWN partial, not a silent drop — even
    /// with a submitted count on hand, the cancelled/partials branch must never synthesize
    /// absentees for items that legitimately never ran (`gemini_poll_cancelled_with_partials_is_done`
    /// covers the same batch with no count at all; this proves a count present doesn't change
    /// the outcome either).
    #[test]
    fn gemini_poll_cancelled_partials_are_not_cross_checked() {
        let v = json!({
            "metadata": { "state": "BATCH_STATE_CANCELLED",
                "output": { "inlinedResponses": { "inlinedResponses": [
                    { "metadata": { "key": "0" }, "response": { "candidates": [ { "content": { "parts": [ { "text": "done in time" } ] } } ] } }
                ] } } }
        });
        // 3 were submitted; only 1 beat the cancel. A submitted count must not turn the
        // other 2 into synthetic "missing" failures — the cancel already explains the gap.
        match parse_gemini_poll(&v, Some(3)).unwrap() {
            BatchPoll::Done(a) => assert_eq!(
                a.len(),
                1,
                "a known partial stays exactly as short as the provider left it: {a:?}"
            ),
            other => panic!("expected Done with partials, got {other:?}"),
        }
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
        let answers = match parse_gemini_poll(&v, None).unwrap() {
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
        let answers = match parse_gemini_poll(&v, None).unwrap() {
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
            parse_gemini_poll(&v, None).unwrap(),
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
        match parse_gemini_poll(&v, None).unwrap() {
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
        assert!(parse_gemini_poll(&v, None).is_err());
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

    /// Dispatch refuses a backend with no batch lane (DeepSeek / OpenRouter / a *local*
    /// openai endpoint) and points at the supported set; `batch_supported` agrees.
    #[test]
    fn dispatch_refuses_unsupported_backends() {
        assert!(batch_supported(&anthropic_backend()));
        assert!(batch_supported(&hosted_openai_backend()));
        for mut b in [
            {
                let mut b = anthropic_backend();
                b.kind = ProviderKind::DeepSeek;
                b
            },
            {
                let mut b = anthropic_backend();
                b.kind = ProviderKind::OpenRouter;
                b
            },
            local_openai_backend(),
        ] {
            b.name = format!("{:?}", b.kind).to_lowercase();
            assert!(!batch_supported(&b), "{:?} has no batch lane", b.kind);
            let slot = ModelSlot::bare(&b.name, "m");
            let err = submitter(&b, &slot, &Defaults::default())
                .err()
                .unwrap_or_else(|| panic!("{:?} must be refused", b.kind));
            let msg = err.to_string();
            assert!(
                msg.contains("no batch lane"),
                "refusal explains the gap: {err}"
            );
            // The refusal names the live supported set, `openai`'s condition included —
            // a bare "openai" would send a local-server user in circles.
            assert!(
                msg.contains("anthropic") && msg.contains("gemini") && msg.contains("openai"),
                "refusal names the live supported set: {err}"
            );
            assert!(poller(&b).is_err());
        }
    }

    // --- OpenAI batch --------------------------------------------------------

    /// A hosted OpenAI Platform backend: kind `openai`, base URL exactly Platform's.
    /// That exact-URL match is the whole capability seam (`Backend::is_hosted_openai`).
    fn hosted_openai_backend() -> Backend {
        Backend {
            name: "gpt".into(),
            kind: ProviderKind::Openai,
            base_url: Some(crate::credentials::HOSTED_OPENAI_BASE_URL.to_string()),
            key_optional: false,
            ..anthropic_backend()
        }
    }

    /// The same kind pointed at a local llama.cpp/Ollama-style server — OpenAI-compatible
    /// on `/chat/completions`, with no Batch API behind it at all.
    fn local_openai_backend() -> Backend {
        Backend {
            name: "openai-local".into(),
            kind: ProviderKind::Openai,
            base_url: Some("http://localhost:8000/v1".into()),
            key_optional: true,
            ..anthropic_backend()
        }
    }

    /// An OpenAI-compatible gateway that proxies `/v1/responses` (`wire = "responses"`),
    /// so its interactive calls take the Responses shape — but has no `/v1/batches` or
    /// Files API behind it, so it must stay refused here regardless.
    fn responses_wire_gateway_backend() -> Backend {
        Backend {
            name: "gateway".into(),
            kind: ProviderKind::Openai,
            base_url: Some("https://llm-gateway.example.internal/v1".into()),
            key_optional: true,
            wire: Some(crate::config::OpenaiWire::Responses),
            ..anthropic_backend()
        }
    }

    /// The `openai` kind's batch verdict is a *backend* property, not a kind property:
    /// same kind, opposite answers, decided by the base URL. This is the regression that
    /// matters — a kind-keyed check would hand a local server a batch lane it can't serve.
    #[test]
    fn openai_batch_lane_is_per_backend_not_per_kind() {
        assert!(batch_supported(&hosted_openai_backend()));
        assert!(!batch_supported(&local_openai_backend()));
        // A trailing slash on the configured URL is the same Platform endpoint.
        let mut slashed = hosted_openai_backend();
        slashed.base_url = Some(format!("{}/", crate::credentials::HOSTED_OPENAI_BASE_URL));
        assert!(batch_supported(&slashed));
    }

    /// `wire = "responses"` is an interactive-shape override, not a batch-eligibility
    /// grant: `is_hosted_openai` — the gate `batch_supported`/`require_hosted` actually
    /// check — stays endpoint-exact regardless of `wire`. A gateway that proxies
    /// `/v1/responses` does not necessarily proxy `/v1/batches` or the Files API, so it
    /// must still be refused a batch lane.
    #[test]
    fn responses_wire_gateway_is_not_batch_eligible() {
        let backend = responses_wire_gateway_backend();
        assert!(
            backend.uses_responses_wire(),
            "fixture must select the Responses shape for interactive calls"
        );
        assert!(
            !batch_supported(&backend),
            "wire = \"responses\" must not make a gateway batch-eligible"
        );
        assert!(OpenaiBatch::from_backend(&backend).is_err());
        assert!(poller(&backend).is_err());
    }

    /// Refusing a local openai backend names the *fix* (Platform's URL), not just the gap —
    /// the user's next move is a one-line config edit, so say which line.
    #[test]
    fn local_openai_batch_refusal_names_the_platform_url() {
        let err = OpenaiBatch::from_backend(&local_openai_backend())
            .err()
            .expect("a local openai endpoint has no batch lane");
        let msg = err.to_string();
        assert!(msg.contains("localhost:8000"), "names the endpoint: {msg}");
        assert!(
            msg.contains(crate::credentials::HOSTED_OPENAI_BASE_URL),
            "names the fix: {msg}"
        );
    }

    /// Batch floors the knobs on hosted OpenAI too, and by the *same* rule as every other
    /// provider — the two shaping functions can't drift on which direction wins. A thin,
    /// `low`-effort slot is lifted to the batch budget and `BATCH_EFFORT`; a slot that
    /// asked deeper keeps its rung; an unrankable rung defers to the operator.
    #[test]
    fn openai_shaping_floors_tokens_and_treats_effort_as_a_floor() {
        let (max_tokens, params) = openai_batch_shaping("gpt-5.6-sol", &tunables("low", 100));
        assert!(
            max_tokens >= BATCH_MAX_TOKENS_FLOOR,
            "floored to the batch budget, got {max_tokens}"
        );
        assert_eq!(
            params.as_ref().unwrap()["reasoning"]["effort"],
            json!(BATCH_EFFORT),
            "the slot's shallower `low` is lifted to the batch effort"
        );
        assert!(
            params.as_ref().unwrap().get("top_p").is_none(),
            "batch leaves sampling to the provider default"
        );
        // A slot already asking for more keeps it — floored, never capped.
        let (bigger, _) = openai_batch_shaping("gpt-5.6-sol", &tunables("low", 1 << 20));
        assert_eq!(bigger, 1 << 20);

        let (_, params) = openai_batch_shaping("gpt-5.6-sol", &tunables("xhigh", 100));
        assert_eq!(
            params.as_ref().unwrap()["reasoning"]["effort"],
            json!("xhigh"),
            "a deeper slot effort survives the batch lane instead of being demoted"
        );

        let (_, params) = openai_batch_shaping("gpt-5.6-sol", &tunables("ludicrous", 100));
        assert_eq!(
            params.as_ref().unwrap()["reasoning"]["effort"],
            json!("ludicrous"),
            "an unrankable rung passes through — kaibo holds no effort allowlist"
        );
    }

    /// The model-aware half: an older chat family takes no `reasoning` block at all, so
    /// batch sends none rather than a 400-bait guess.
    #[test]
    fn openai_shaping_omits_reasoning_for_non_reasoning_families() {
        let (_, params) = openai_batch_shaping("gpt-4.1-mini", &tunables("high", 100));
        assert!(
            params.is_none(),
            "no reasoning block, and batch sends no sampling: {params:?}"
        );
    }

    /// The JSONL input file's wire shape, pinned: one line per item, each a self-contained
    /// Responses request carrying the correlation id, the endpoint OpenAI replays, and the
    /// maxed body.
    #[test]
    fn openai_jsonl_pins_the_request_shape() {
        let (max_tokens, params) = openai_batch_shaping("gpt-5.6-sol", &tunables("low", 100));
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
        let jsonl = openai_batch_jsonl("gpt-5.6-sol", max_tokens, "be terse", &params, &[], &items)
            .unwrap();
        let lines: Vec<Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
            .collect();
        assert_eq!(lines.len(), 2, "one line per item");
        assert_eq!(lines[0]["custom_id"], json!("0"));
        assert_eq!(lines[1]["custom_id"], json!("1"));
        assert_eq!(lines[0]["method"], json!("POST"));
        assert_eq!(lines[0]["url"], json!("/v1/responses"));
        let body = &lines[0]["body"];
        assert_eq!(body["model"], json!("gpt-5.6-sol"));
        assert_eq!(body["instructions"], json!("be terse"));
        assert_eq!(
            body["input"],
            json!("first"),
            "no attachments stays the bare-string shorthand"
        );
        assert_eq!(body["max_output_tokens"], json!(max_tokens));
        assert_eq!(
            body["reasoning"]["effort"],
            json!(BATCH_EFFORT),
            "shaping params are flattened into the request body"
        );
        assert!(
            jsonl.ends_with('\n'),
            "JSONL is newline-terminated, not newline-separated"
        );
    }

    /// Shared attachments ride ahead of every item's prompt as native Responses parts:
    /// text under its `<file>` wrapper, an image as a data URL, the prompt last.
    #[test]
    fn openai_jsonl_carries_attachments_as_responses_parts() {
        let attachments = vec![
            Attachment::Text {
                path: "src/lib.rs".into(),
                body: "fn main() {}".into(),
            },
            Attachment::Image {
                path: "diagram.png".into(),
                mime: "image/png",
                data_b64: "AAAA".into(),
            },
        ];
        let items = vec![BatchItem {
            custom_id: "0".into(),
            prompt: "review it".into(),
        }];
        let jsonl =
            openai_batch_jsonl("gpt-5.6-sol", 4096, "", &None, &attachments, &items).unwrap();
        let line: Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert!(
            line["body"].get("instructions").is_none(),
            "an empty preamble sends no `instructions` key"
        );
        let content = &line["body"]["input"][0]["content"];
        assert_eq!(line["body"]["input"][0]["role"], json!("user"));
        assert_eq!(content[0]["type"], json!("input_text"));
        assert!(
            content[0]["text"].as_str().unwrap().contains("src/lib.rs"),
            "the text attachment keeps its <file> wrapper: {content}"
        );
        assert_eq!(content[1]["type"], json!("input_image"));
        assert_eq!(
            content[1]["image_url"],
            json!("data:image/png;base64,AAAA"),
            "images ride as data URLs"
        );
        assert_eq!(
            content[2],
            json!({ "type": "input_text", "text": "review it" }),
            "the prompt comes last, after its context"
        );
    }

    fn openai_batch_object(status: &str, extra: Value) -> Value {
        let mut v = json!({
            "id": "batch_abc",
            "object": "batch",
            "endpoint": "/v1/responses",
            "status": status,
            "request_counts": { "total": 4, "completed": 3, "failed": 1 },
        });
        if let (Value::Object(base), Value::Object(more)) = (&mut v, extra) {
            base.extend(more);
        }
        v
    }

    /// The status map, pinned across OpenAI's whole lifecycle. `completed` sums the
    /// terminal buckets — a batch whose items *failed* still counts them as done, or the
    /// progress line would stall short of its total forever.
    #[test]
    fn openai_status_maps_the_lifecycle() {
        for pending in ["validating", "in_progress", "finalizing"] {
            assert_eq!(
                parse_openai_status(&openai_batch_object(pending, json!({}))).unwrap(),
                OpenaiStatus::Pending {
                    completed: 4,
                    total: 4
                },
                "{pending} is progress, not a terminal state"
            );
        }
        assert_eq!(
            parse_openai_status(&openai_batch_object("cancelling", json!({}))).unwrap(),
            OpenaiStatus::Cancelling
        );
        assert_eq!(
            parse_openai_status(&openai_batch_object(
                "completed",
                json!({ "output_file_id": "file-out", "error_file_id": "file-err" })
            ))
            .unwrap(),
            OpenaiStatus::Ended {
                state: "completed".into(),
                output_file_id: Some("file-out".into()),
                error_file_id: Some("file-err".into()),
            }
        );
        // A cancelled/expired batch can still carry the items that finished in time.
        assert_eq!(
            parse_openai_status(&openai_batch_object(
                "expired",
                json!({ "output_file_id": "file-out" })
            ))
            .unwrap(),
            OpenaiStatus::Ended {
                state: "expired".into(),
                output_file_id: Some("file-out".into()),
                error_file_id: None,
            }
        );
        // An unknown status is loud — OpenAI's set is closed, so a new member is news.
        let err = parse_openai_status(&openai_batch_object("teleporting", json!({})))
            .expect_err("unknown status is refused");
        assert!(err.to_string().contains("teleporting"), "{err}");
    }

    /// An empty string file id reads as absent, not as a downloadable file — OpenAI sends
    /// `""` (not `null`) for a batch that produced no errors, and fetching `/files//content`
    /// would 404 the whole poll.
    #[test]
    fn openai_status_treats_empty_file_ids_as_absent() {
        assert_eq!(
            parse_openai_status(&openai_batch_object(
                "completed",
                json!({ "output_file_id": "file-out", "error_file_id": "" })
            ))
            .unwrap(),
            OpenaiStatus::Ended {
                state: "completed".into(),
                output_file_id: Some("file-out".into()),
                error_file_id: None,
            }
        );
    }

    fn openai_result_line(custom_id: &str, body: Value) -> String {
        json!({
            "id": "batch_req_1",
            "custom_id": custom_id,
            "response": { "status_code": 200, "request_id": "req_1", "body": body },
            "error": null,
        })
        .to_string()
    }

    /// A succeeded item's answer is the assistant message's `output_text`, with reasoning
    /// items skipped — the scratchpad is not the answer.
    #[test]
    fn openai_results_join_output_text_and_skip_reasoning() {
        let body = json!({
            "status": "completed",
            "output": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "thinking…" }] },
                { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "the " },
                    { "type": "output_text", "text": "answer" },
                ]},
            ],
        });
        let answers = parse_openai_output_jsonl(&openai_result_line("0", body)).unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].custom_id, "0");
        assert_eq!(answers[0].text.as_deref(), Ok("the answer"));
    }

    /// A truncated answer is kept but announces itself — never presented as a finished one.
    #[test]
    fn openai_incomplete_response_is_banner_gated() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [{ "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "half an ans" }
            ]}],
        });
        let answers = parse_openai_output_jsonl(&openai_result_line("0", body)).unwrap();
        let text = answers[0].text.as_ref().expect("partial text is kept");
        assert!(text.contains("INCOMPLETE"), "{text}");
        assert!(text.contains("max_output_tokens"), "{text}");
        assert!(
            text.contains("half an ans"),
            "the fragment survives: {text}"
        );

        // …and with no text at all it's an honest per-item failure, not an empty success.
        let empty = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [{ "type": "reasoning", "summary": [] }],
        });
        let answers = parse_openai_output_jsonl(&openai_result_line("0", empty)).unwrap();
        assert!(answers[0].text.is_err(), "{:?}", answers[0].text);
    }

    /// A completed-but-empty answer is no answer — the same gate the other providers pass.
    #[test]
    fn openai_empty_completed_answer_is_a_failure() {
        let body = json!({ "status": "completed", "output": [] });
        let answers = parse_openai_output_jsonl(&openai_result_line("0", body)).unwrap();
        assert_eq!(
            answers[0].text.as_ref().unwrap_err(),
            "model returned an empty answer"
        );
    }

    /// The error file's lines and non-200 per-item responses both land as per-item
    /// failures with the provider's own reason — one bad item never sinks the batch, and
    /// never silently vanishes either.
    #[test]
    fn openai_per_item_failures_surface_their_reason() {
        let request_error = json!({
            "id": "batch_req_2",
            "custom_id": "1",
            "response": null,
            "error": { "code": "invalid_request", "message": "model not found" },
        })
        .to_string();
        let http_error = json!({
            "id": "batch_req_3",
            "custom_id": "2",
            "response": { "status_code": 429, "request_id": "req_3", "body": {
                "error": { "message": "rate limit reached", "type": "rate_limit_error" }
            }},
            "error": null,
        })
        .to_string();
        let answers =
            parse_openai_output_jsonl(&format!("{request_error}\n{http_error}\n")).unwrap();
        assert!(answers[0]
            .text
            .as_ref()
            .unwrap_err()
            .contains("model not found"));
        let http = answers[1].text.as_ref().unwrap_err();
        assert!(http.contains("429"), "{http}");
        assert!(http.contains("rate limit reached"), "{http}");
    }

    /// Succeeded and failed items come back in *separate files*, so the merged set is
    /// re-ordered by kaibo's own index ids — the caller's submit order, restored.
    #[test]
    fn openai_results_are_reordered_by_submit_index() {
        let merged = order_by_index(vec![
            BatchAnswer {
                custom_id: "2".into(),
                text: Ok("third".into()),
            },
            BatchAnswer {
                custom_id: "0".into(),
                text: Ok("first".into()),
            },
            BatchAnswer {
                custom_id: "1".into(),
                text: Err("failed".into()),
            },
        ]);
        let ids: Vec<&str> = merged.iter().map(|a| a.custom_id.as_str()).collect();
        assert_eq!(ids, ["0", "1", "2"]);
        // Numeric sort, not lexicographic: "10" comes after "9".
        let many = order_by_index(
            ["9", "10", "1"]
                .into_iter()
                .map(|id| BatchAnswer {
                    custom_id: id.into(),
                    text: Ok(String::new()),
                })
                .collect(),
        );
        let ids: Vec<&str> = many.iter().map(|a| a.custom_id.as_str()).collect();
        assert_eq!(ids, ["1", "9", "10"]);
        // Non-numeric ids keep the provider's order rather than being reshuffled.
        let opaque = order_by_index(
            ["zeta", "alpha"]
                .into_iter()
                .map(|id| BatchAnswer {
                    custom_id: id.into(),
                    text: Ok(String::new()),
                })
                .collect(),
        );
        let ids: Vec<&str> = opaque.iter().map(|a| a.custom_id.as_str()).collect();
        assert_eq!(ids, ["zeta", "alpha"]);
    }

    /// `docs/issues.md`'s P3 batch cross-check, OpenAI side: a per-file parse can't judge
    /// completeness on its own (a missing id might just be sitting in the *other* file),
    /// so the check runs once, on the merged set, in [`finalize_openai_answers`] — and
    /// only for `completed`, the one state that promises the two files together cover
    /// every submitted item. Submitting 3 but merging 2 (both from the output file here)
    /// must synthesize the third as a loud per-item failure.
    #[test]
    fn finalize_openai_answers_cross_checks_a_completed_batch() {
        let answers = vec![
            BatchAnswer {
                custom_id: "0".into(),
                text: Ok("first".into()),
            },
            BatchAnswer {
                custom_id: "1".into(),
                text: Ok("second".into()),
            },
            // custom_id "2" never came back, in either file.
        ];
        let out = finalize_openai_answers(answers, "completed", Some(3));
        assert_eq!(out.len(), 3, "the gap is filled: {out:?}");
        assert_eq!(out[2].custom_id, "2");
        let msg = out[2].text.as_ref().expect_err("missing item is a failure");
        assert!(
            msg.contains("kaibo"),
            "names itself, not the provider: {msg}"
        );
        assert!(
            !msg.contains("provider error:"),
            "must not read like a provider-reported failure: {msg}"
        );
    }

    /// `expired`/`cancelled` legitimately hand back a partial result set (whatever beat the
    /// clock/cancel) — cross-checking those against the full submitted count would
    /// misreport a known partial as kaibo's own "missing" absentee, so those states must
    /// stay exactly as short as the provider left them even with a submitted count on hand.
    #[test]
    fn finalize_openai_answers_skips_the_check_on_a_known_partial() {
        let answers = vec![BatchAnswer {
            custom_id: "0".into(),
            text: Ok("done in time".into()),
        }];
        for state in ["expired", "cancelled"] {
            let out = finalize_openai_answers(answers.clone(), state, Some(3));
            assert_eq!(
                out.len(),
                1,
                "a known partial ({state}) stays exactly as short as the provider left it: {out:?}"
            );
        }
    }

    /// A batch that fails input validation has no result files at all — the reason lives
    /// on the batch object, with the offending JSONL line.
    #[test]
    fn openai_validation_failure_reads_its_reason_off_the_batch() {
        let v = openai_batch_object(
            "failed",
            json!({ "errors": { "object": "list", "data": [
                { "code": "invalid_json_line", "message": "not valid JSON", "line": 3 },
                { "code": "model_not_found", "message": "unknown model" },
            ]}}),
        );
        let text = openai_batch_error_text(&v).expect("a failed batch names its reason");
        assert!(text.contains("line 3: not valid JSON"), "{text}");
        assert!(text.contains("unknown model"), "{text}");
        // No `errors` block at all is None, so the poll can say *that* instead of "".
        assert_eq!(
            openai_batch_error_text(&openai_batch_object("failed", json!({}))),
            None
        );
    }

    /// The list view: epoch seconds become the RFC3339 the recency filter reads back, and
    /// `has_more` rides along so a truncated page announces itself.
    #[test]
    fn openai_list_renders_timestamps_the_recency_filter_can_read() {
        let page = json!({
            "object": "list",
            "data": [openai_batch_object("in_progress", json!({ "created_at": 946_684_800 }))],
            "has_more": true,
        });
        let (items, has_more) = parse_openai_list(&page).unwrap();
        assert!(has_more);
        assert_eq!(items[0].provider_id, "batch_abc");
        assert_eq!(items[0].status, "in_progress");
        assert_eq!((items[0].completed, items[0].total), (4, 4));
        let created = items[0].created_at.as_deref().expect("a dated batch");
        assert_eq!(
            rfc3339_to_epoch(created),
            Some(946_684_800),
            "the round trip the `list` recency filter depends on: {created}"
        );
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
            provider.poll(&id, None).await.unwrap(),
            BatchPoll::Pending { .. }
        ));
        assert!(matches!(
            provider.poll(&id, None).await.unwrap(),
            BatchPoll::Done(_)
        ));
        // Over-polling a Done batch stays Done.
        assert!(matches!(
            provider.poll(&id, None).await.unwrap(),
            BatchPoll::Done(_)
        ));
        provider.cancel(&id).await.unwrap();
        assert_eq!(provider.canceled(), vec!["msgbatch_test".to_string()]);
    }
}
