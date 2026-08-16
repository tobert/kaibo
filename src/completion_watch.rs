//! One record per completion. Wrap a phase's completion model so every turn's
//! provider response is *seen* on its way past — most of all the **finish reason**,
//! which rig's agent layer discards before kaibo can read it.
//!
//! The gap this closes: `rig_core::completion::CompletionResponse<T>` carries
//! `raw_response: T`, the untouched provider payload, but the agent hook event
//! (`rig-agent`'s `CompletionCall`) is a deliberately medium-neutral shape — prompt,
//! content, usage, message id, and no raw response. So a consult that came back
//! empty was reported as a plain success and nothing on the wire could tell an early
//! stop from a `content_filter` refusal from a truncation. `CompletionModel` is a
//! *trait*, and `Agent<M: CompletionModel>` is generic over it, so wrapping the model
//! the loop calls puts us on the one path every turn takes — including the turns
//! *inside* the tool loop, which no hook reaches.
//!
//! This is [`crate::tool_span`]'s sibling: `traced` wraps a tool so each call is
//! observed, [`watched`] wraps a model so each completion is. Both are transparent —
//! [`Watched`] forwards `completion`/`stream` verbatim and returns the provider's
//! response and errors untouched, so nothing about a call changes except that we now
//! know how it ended.
//!
//! **Provider-agnostic by construction, not by a match arm per backend.** The trait
//! bounds require `type Response: Serialize + DeserializeOwned`, so the wrapper
//! serializes the raw response and reads the finish reason out of the JSON under the
//! spellings providers actually ship ([`finish_reason`]). An unfamiliar shape yields
//! `None` — "we don't know", never a broken call — which is the right degrade for a
//! gateway or a provider rig grows next month.
//!
//! **What we keep, and what we let go.** Per turn: the finish reason, the reported
//! [`Usage`], and two cheap counts read straight off `choice` (assistant text length,
//! tool calls emitted) — together they say "this turn produced nothing, and here is
//! why". We deliberately do *not* retain the raw JSON: it rides every turn of every
//! phase and carries the full assistant message (and, on some providers, the echoed
//! reasoning), so holding it would grow with the transcript for a payload nothing
//! reads. The extraction is the point; the haystack is not worth keeping.

use std::sync::{Arc, Mutex};

use rig_core::completion::message::AssistantContent;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
};
use serde::Serialize;
use serde_json::Value;

/// The field names a provider spells its finish reason with, in preference order.
///
/// Confirmed against the vendored rig-core 0.41 provider sources rather than
/// guessed — each entry names where it lives:
///
/// - `finish_reason` — the OpenAI-compatible chat-completions `Choice`
///   (`providers/openai/completion/mod.rs`, `providers/deepseek.rs`,
///   `providers/openrouter/completion.rs`), nested under `choices[]`. OpenRouter
///   carries a `native_finish_reason` beside it; we take the normalized one.
/// - `stop_reason` — Anthropic's Messages response, at the top level
///   (`providers/anthropic/completion.rs`). `Option<String>`, so it can be null.
/// - `finishReason` — Gemini's `ContentCandidate` under `candidates[]`, camelCased
///   by `#[serde(rename_all = "camelCase")]`, with `SCREAMING_SNAKE_CASE` values
///   (`STOP`, `MAX_TOKENS`, `SAFETY`, …) (`providers/gemini/completion.rs`).
/// - `stopReason` — the camelCase spelling an Anthropic-compatible gateway may use.
/// - `incomplete_details` / `incompleteDetails` — OpenAI's **Responses** API, which
///   has no finish-reason field at all: it reports `status` plus, when the run did
///   not finish cleanly, `incomplete_details: { reason: "max_output_tokens" | … }`
///   (`providers/openai/responses_api/mod.rs`). We read that object's `reason`, so a
///   *completed* Responses call reports no finish reason — absence is the normal
///   stop there. `status` is deliberately left out: `"completed"` is noise as a
///   finish reason, and the word also appears on per-item tool statuses deeper in
///   the same payload.
const FINISH_KEYS: &[&str] = &[
    "finish_reason",
    "stop_reason",
    "finishReason",
    "stopReason",
    "incomplete_details",
    "incompleteDetails",
];

/// How deep to look for a finish reason. Every shape above puts it at depth 0
/// (Anthropic), 1 (the Responses object), or 2 (`choices[]` / `candidates[]`); the
/// cap keeps an unfamiliar payload from turning observation into a deep walk.
const MAX_DEPTH: usize = 4;

/// Read a provider's finish reason out of its serialized raw response.
///
/// Breadth-first, so the shallowest match wins — Anthropic's top-level `stop_reason`
/// is found before anything nested, and an OpenAI-compatible `choices[0]` is reached
/// on the next level. A key whose value is a string *is* the reason; a key whose
/// value is an object with a string `reason` (OpenAI Responses' `incomplete_details`)
/// yields that. Anything else — a null (Anthropic's unfinished case), a missing key,
/// a shape we've never seen — keeps the search going and finally yields `None`.
///
/// Returning `None` is a real answer: "this response reports no finish reason".
/// It is what an unknown provider degrades to, and it is what a clean OpenAI
/// Responses call genuinely says.
pub fn finish_reason(raw: &Value) -> Option<String> {
    let mut frontier = vec![raw];
    for _ in 0..=MAX_DEPTH {
        let mut next: Vec<&Value> = Vec::new();
        for node in frontier {
            match node {
                Value::Object(map) => {
                    for key in FINISH_KEYS {
                        match map.get(*key) {
                            Some(Value::String(s)) => return Some(s.clone()),
                            // `incomplete_details: { reason: "max_output_tokens" }`
                            Some(Value::Object(inner)) => {
                                if let Some(Value::String(s)) = inner.get("reason") {
                                    return Some(s.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    next.extend(map.values());
                }
                Value::Array(items) => next.extend(items.iter()),
                _ => {}
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    None
}

/// What one completion told us, once the provider-specific wrapping is off.
///
/// `finish_reason` is the reason this exists — the field that separates "the model
/// answered" from "the provider truncated us" from "a classifier refused". The rest
/// is the cheap context that makes it actionable: an empty answer with
/// `text_chars = 0`, `tool_calls = 0`, and `finish_reason = Some("content_filter")`
/// is a diagnosis, where any one of the three alone is a guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnRecord {
    /// The provider's own word for why generation stopped, verbatim — `"stop"`,
    /// `"max_tokens"`, `"MAX_TOKENS"`, `"content_filter"`, `"tool_use"`, … Never
    /// normalized: the vocabulary differs per provider and flattening it would throw
    /// away exactly the detail a diagnosis needs. `None` means this response reported
    /// none (see [`finish_reason`]).
    pub finish_reason: Option<String>,
    /// The token usage the provider reported for *this* completion. rig sums these
    /// across a run; here they stay per turn, so a run's spend can be read turn by
    /// turn. Zero-valued is rig's documented "the provider reported nothing".
    pub usage: Usage,
    /// Characters of assistant text this turn emitted (0 for a pure tool-call turn,
    /// and for the empty answer this module exists to explain).
    pub text_chars: usize,
    /// Tool calls this turn emitted.
    pub tool_calls: usize,
}

impl TurnRecord {
    /// Read one response. The raw payload is serialized here and dropped immediately;
    /// only the extraction survives (see the module doc on what we keep).
    ///
    /// A raw response that refuses to serialize degrades to "no finish reason" and a
    /// debug log — observation must never fail a completion the caller already paid
    /// for. That is the *only* thing this swallows, and it is loud in the log.
    fn observe<T: Serialize>(response: &CompletionResponse<T>) -> Self {
        let raw = match serde_json::to_value(&response.raw_response) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::debug!(
                    %error,
                    "raw provider response would not serialize; finish reason unavailable"
                );
                Value::Null
            }
        };
        let mut text_chars = 0;
        let mut tool_calls = 0;
        for content in response.choice.iter() {
            match content {
                AssistantContent::Text(text) => text_chars += text.text.chars().count(),
                AssistantContent::ToolCall(_) => tool_calls += 1,
                _ => {}
            }
        }
        Self {
            finish_reason: finish_reason(&raw),
            usage: response.usage,
            text_chars,
            tool_calls,
        }
    }
}

/// The conventions' `error.type` for a failed completion — the failure's *class*, from
/// rig's own error enum.
///
/// The variant name, never the message. A metric attribute mints a time series per
/// distinct value, and every one of these variants carries a formatted string holding
/// a URL, a provider's error body, or a serde path — unbounded cardinality, and content
/// on the signal built to carry none. Seven variants is the right resolution for "what
/// kind of thing went wrong", and the message is still in the error the caller gets.
fn completion_error_type(error: &CompletionError) -> &'static str {
    match error {
        CompletionError::HttpError(_) => "http_error",
        CompletionError::JsonError(_) => "json_error",
        CompletionError::UrlError(_) => "url_error",
        CompletionError::RequestError(_) => "request_error",
        CompletionError::ResponseError(_) => "response_error",
        CompletionError::ProviderError(_) => "provider_error",
        CompletionError::ProviderResponse(_) => "provider_response_error",
        // rig's enum is `#[non_exhaustive]`. A variant added upstream lands here
        // rather than failing our build, and "other" is the honest label for a class
        // we have not named yet — the metric keeps working across a rig bump.
        _ => "other",
    }
}

/// The per-call slot [`Watched`] records into: every completion of one phase, in
/// order. Cheaply cloneable and `Send + Sync`, because the model is cloned into each
/// agent rig builds (once per loop iteration, again for the forced final turn) and
/// every clone must land in the *same* log.
///
/// **Scoped to one call, never global.** A phase makes its own log and hands it to
/// its own wrapper, so two concurrent consults never see each other's turns. It has
/// nothing to do with the kaish kernel — the log holds plain data, crosses `.await`
/// freely, and never touches the `!Send` kernel behind `KaishWorker`.
#[derive(Clone, Default, Debug)]
pub struct CompletionLog(Arc<Mutex<Vec<TurnRecord>>>);

impl CompletionLog {
    /// An empty log for one phase call.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every completion recorded so far, oldest first — tool-loop turns included.
    pub fn turns(&self) -> Vec<TurnRecord> {
        self.0.lock().expect("completion log poisoned").clone()
    }

    /// The most recent completion — the turn that produced the answer (or failed to).
    pub fn last(&self) -> Option<TurnRecord> {
        self.0
            .lock()
            .expect("completion log poisoned")
            .last()
            .cloned()
    }

    /// The most recent completion's finish reason, if it reported one. The one-liner
    /// a caller reaches for when asking "why did this answer come back empty?".
    pub fn last_finish_reason(&self) -> Option<String> {
        self.last().and_then(|turn| turn.finish_reason)
    }

    /// How many completions this phase has made — every turn of the tool loop, plus
    /// any forced final turn.
    pub fn len(&self) -> usize {
        self.0.lock().expect("completion log poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&self, turn: TurnRecord) {
        self.0.lock().expect("completion log poisoned").push(turn);
    }
}

/// A completion model that records what each response reported, then hands the
/// response back untouched.
///
/// Pure passthrough by construction: `completion` awaits the inner model, reads the
/// response, and returns the very same `Ok`/`Err`. Output, usage, `message_id`,
/// `raw_response`, and every error are identical to the unwrapped model's — the only
/// difference is that a [`TurnRecord`] landed in the log.
#[derive(Clone, Debug)]
pub struct Watched<M> {
    inner: M,
    log: CompletionLog,
    /// Who this arm calls, for the GenAI client metrics. `None` for an arm with no
    /// provider behind it — the offline scripted client — which records nothing; see
    /// [`crate::metrics::PhaseIdentity`].
    ident: Option<MetricIdent>,
}

/// The owned half of [`crate::metrics::CallIdent`]. Owned because this wrapper is
/// cloned into every agent rig builds and must outlive the call site that named it.
#[derive(Clone, Debug)]
struct MetricIdent {
    provider: crate::credentials::ProviderKind,
    model: String,
}

impl<M> Watched<M> {
    /// Wrap `model` so its completions record into `log`, and — when the arm names a
    /// provider — into the client metrics.
    pub fn new(
        model: M,
        log: CompletionLog,
        ident: Option<crate::metrics::PhaseIdentity>,
        model_name: &str,
    ) -> Self {
        Self {
            inner: model,
            log,
            ident: ident.map(|i| MetricIdent {
                provider: i.provider,
                model: model_name.to_string(),
            }),
        }
    }
}

/// Wrap a completion model so every turn records into `log` — the drop-in at an
/// agent-builder call site, mirroring [`traced`](crate::tool_span::traced) for tools.
pub fn watched<M: CompletionModel>(
    model: M,
    log: CompletionLog,
    ident: Option<crate::metrics::PhaseIdentity>,
    model_name: &str,
) -> Watched<M> {
    Watched::new(model, log, ident, model_name)
}

impl<M: CompletionModel> CompletionModel for Watched<M> {
    type Response = M::Response;
    type StreamingResponse = M::StreamingResponse;
    type Client = M::Client;

    /// Unreachable, and loudly so. `make` is only ever called through
    /// `CompletionClient::completion_model`, and no client's `CompletionModel` is a
    /// `Watched` — kaibo always wraps an already-built model with [`watched`]. Building
    /// one here would have to invent a log nobody holds, silently discarding every
    /// observation; a panic names the correct constructor instead.
    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unreachable!(
            "Watched is built by `watched(model, log, ..)`, never by CompletionModel::make"
        )
    }

    /// Still a pure passthrough: the response and every error are returned untouched.
    /// What it now also does is *time* the call, because this is the one place kaibo
    /// sees a single provider request begin and end — rig's agent hook fires per turn
    /// of the outer loop, which is a different bracket once a turn retries.
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let started = std::time::Instant::now();
        let result = self.inner.completion(request).await;
        let elapsed = started.elapsed();

        if let Some(ident) = &self.ident {
            let call = crate::metrics::CallIdent {
                provider: ident.provider,
                model: &ident.model,
            };
            match &result {
                // A failed call has no usage to report, but its duration is the
                // number an operator most wants when a provider is degrading.
                Err(error) => {
                    crate::metrics::record_completion(
                        call,
                        elapsed,
                        None,
                        Some(completion_error_type(error)),
                    );
                }
                Ok(response) => {
                    crate::metrics::record_completion(call, elapsed, Some(&response.usage), None);
                }
            }
        }

        let response = result?;
        self.log.push(TurnRecord::observe(&response));
        Ok(response)
    }

    /// Forwarded verbatim. kaibo drives the non-streaming loop, so nothing here calls
    /// this today; a streamed response reports its finish reason through rig's own
    /// stream events rather than a single raw payload, so wrapping it would be a
    /// different job than this one.
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<
        rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
        CompletionError,
    > {
        self.inner.stream(request).await
    }

    /// Forwarded: this is a provider capability, and the wrapper must not change what
    /// rig believes the model can do.
    fn composes_native_output_with_tools(&self) -> bool {
        self.inner.composes_native_output_with_tools()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        provider_error, text_response, tool_call_response, usage, with_raw, with_usage,
        ScriptedClient, ScriptedModel,
    };
    use rig_core::client::CompletionClient;
    use rig_core::message::Message;
    use rig_core::OneOrMany;
    use serde_json::json;

    /// A one-turn request, the shape rig's agent builds for a toolless call.
    fn req() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::user("q")),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        }
    }

    /// A scripted model whose every call answers with `respond`.
    fn model<F>(respond: F) -> ScriptedModel
    where
        F: Fn(&CompletionRequest) -> Result<CompletionResponse<Value>, CompletionError>
            + Send
            + Sync
            + 'static,
    {
        ScriptedClient::builder()
            .on_model("m", respond)
            .build()
            .completion_model("m")
    }

    /// The shape each provider's raw response takes on the wire, as rig's own
    /// response structs serialize it — the fixtures the extractor must handle.
    /// Trimmed to the fields under test; the nesting is the real nesting.
    fn anthropic_raw(stop: Value) -> Value {
        json!({
            "id": "msg_1",
            "model": "claude-sonnet-4-6",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": stop,
            "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })
    }

    fn openai_raw(finish: &str) -> Value {
        json!({
            "id": "chatcmpl-1",
            "model": "gpt-5.6",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "logprobs": null,
                "finish_reason": finish
            }]
        })
    }

    fn gemini_raw(finish: &str) -> Value {
        json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": finish
            }],
            "usageMetadata": {"promptTokenCount": 3}
        })
    }

    /// The OpenAI **Responses** wire, which has no finish-reason field: a clean run
    /// is `status: completed` with `incomplete_details: null`; a truncated one names
    /// its reason there.
    fn responses_raw(incomplete: Value) -> Value {
        json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "status": if incomplete.is_null() { "completed" } else { "incomplete" },
            "error": null,
            "incomplete_details": incomplete,
            "model": "gpt-5.6-terra",
            "output": [{"type": "message", "status": "completed",
                        "content": [{"type": "output_text", "text": "hi"}]}]
        })
    }

    /// The whole point, across the spellings kaibo's five provider kinds actually
    /// ship: one extractor, no per-provider branch, each nesting found where it
    /// really sits.
    #[test]
    fn reads_the_finish_reason_from_every_provider_spelling() {
        // Anthropic: `stop_reason`, top level.
        assert_eq!(
            finish_reason(&anthropic_raw(json!("max_tokens"))),
            Some("max_tokens".to_string())
        );
        // OpenAI-compatible chat completions (openai, deepseek): `choices[].finish_reason`.
        assert_eq!(
            finish_reason(&openai_raw("content_filter")),
            Some("content_filter".to_string())
        );
        // Gemini: camelCased, SCREAMING_SNAKE valued, under `candidates[]`.
        assert_eq!(
            finish_reason(&gemini_raw("MAX_TOKENS")),
            Some("MAX_TOKENS".to_string())
        );
        // OpenAI Responses: the reason hides inside `incomplete_details`.
        assert_eq!(
            finish_reason(&responses_raw(json!({"reason": "max_output_tokens"}))),
            Some("max_output_tokens".to_string())
        );
    }

    /// OpenRouter reports both a normalized and a native reason on the same choice.
    /// We take the normalized one — the vocabulary the other four kinds share.
    #[test]
    fn prefers_the_normalized_reason_over_openrouter_native() {
        let raw = json!({
            "choices": [{
                "index": 0,
                "native_finish_reason": "COMPLETE",
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }]
        });
        assert_eq!(finish_reason(&raw), Some("stop".to_string()));
    }

    /// An unrecognized payload — a gateway rig has never seen, a provider that
    /// reports nothing, Anthropic's null, a clean Responses call — degrades to "no
    /// finish reason". Never an error, never a guess.
    #[test]
    fn an_unknown_shape_degrades_to_no_finish_reason() {
        assert_eq!(finish_reason(&json!({"result": "who knows"})), None);
        assert_eq!(finish_reason(&Value::Null), None);
        assert_eq!(finish_reason(&json!([1, 2, 3])), None);
        // The unit raw response our scripted client uses.
        assert_eq!(finish_reason(&json!(null)), None);
        // Anthropic's `stop_reason: null` — present, unset.
        assert_eq!(finish_reason(&anthropic_raw(Value::Null)), None);
        // A completed Responses call genuinely reports none.
        assert_eq!(finish_reason(&responses_raw(Value::Null)), None);
    }

    /// A finish reason buried past the cap isn't hunted for — observation stays
    /// cheap on a payload we don't recognize.
    #[test]
    fn the_search_is_depth_capped() {
        let mut deep = json!({"finish_reason": "stop"});
        for _ in 0..MAX_DEPTH + 1 {
            deep = json!({ "nested": deep });
        }
        assert_eq!(finish_reason(&deep), None);
    }

    /// Drive the wrapper the way rig does — one `completion()` call — and prove it
    /// changes nothing observable: same content, same usage, same `message_id`, same
    /// raw response. Only the log is new.
    #[tokio::test]
    async fn the_wrapper_is_a_pure_passthrough() {
        let model = model(|_req| {
            Ok(with_raw(
                with_usage(text_response("the answer"), usage(11, 7)),
                openai_raw("stop"),
            ))
        });

        let bare = model.completion(req()).await.expect("bare completion");
        let log = CompletionLog::new();
        let wrapped = watched(model.clone(), log.clone(), None, "scripted")
            .completion(req())
            .await
            .expect("wrapped completion");

        assert_eq!(
            serde_json::to_value(&wrapped.choice).unwrap(),
            serde_json::to_value(&bare.choice).unwrap(),
            "the wrapper must not touch the content"
        );
        assert_eq!(wrapped.usage, bare.usage, "usage passes through");
        assert_eq!(
            wrapped.message_id, bare.message_id,
            "message id passes through"
        );
        assert_eq!(
            wrapped.raw_response, bare.raw_response,
            "the raw provider response is handed back untouched"
        );
        assert_eq!(
            log.turns(),
            vec![TurnRecord {
                finish_reason: Some("stop".to_string()),
                usage: usage(11, 7),
                text_chars: "the answer".len(),
                tool_calls: 0,
            }],
            "one turn recorded, with the reason the provider gave"
        );
    }

    /// An error from the provider stays an error, verbatim, and records nothing —
    /// there is no response to read.
    #[tokio::test]
    async fn an_error_passes_through_unchanged() {
        let model = model(|_req| Err(provider_error("overloaded_error")));
        let log = CompletionLog::new();

        let bare = model.completion(req()).await;
        let wrapped = watched(model.clone(), log.clone(), None, "scripted")
            .completion(req())
            .await;

        assert_eq!(
            format!("{:?}", wrapped.unwrap_err()),
            format!("{:?}", bare.unwrap_err()),
            "the provider's error reaches the caller unchanged"
        );
        assert!(log.is_empty(), "a failed call records no turn");
    }

    /// A tool-call turn is recorded too — no text, one call, and whatever the
    /// provider said about stopping. This is the turn a hook could see only in a
    /// medium-neutral form, and the loop's inner turns are all this shape.
    #[tokio::test]
    async fn a_tool_call_turn_is_recorded_with_no_text() {
        let model = model(|_req| {
            Ok(with_raw(
                tool_call_response("c1", "run_kaish", json!({"script": "ls"})),
                anthropic_raw(json!("tool_use")),
            ))
        });
        let log = CompletionLog::new();
        watched(model, log.clone(), None, "scripted")
            .completion(req())
            .await
            .expect("completion");

        let turn = log.last().expect("one turn");
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(turn.text_chars, 0);
        assert_eq!(turn.tool_calls, 1);
    }
}
