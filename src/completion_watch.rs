//! One record per completion. Wrap a phase's completion model so every turn's
//! provider response is *seen* on its way past — most of all the **finish reason**,
//! which rig's agent layer discards before kaibo can read it.
//!
//! The gap this closes: `rig_core::completion::CompletionResponse` carries `raw`,
//! the provider's own payload serialized, but the agent hook event
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
//! **Provider-agnostic by construction, not by a match arm per backend.** rig fills
//! `raw` at every provider seam, so the wrapper reads the finish reason out of that
//! JSON under the spellings providers actually ship ([`finish_reason`]). rig 0.42 also
//! carries a normalized `finish_reason()`; kaibo keeps the provider's own word, since
//! the normalized one flattens exactly the detail a diagnosis needs. An unfamiliar shape yields
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rig_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
};
use rig_core::completion::message::AssistantContent;
use rig_core::completion::{CompletionRequest, CompletionResponse, Usage};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::error::{ErrorReport, ProviderError};
use rig_core::operation::Completion;

use rig_core::DynModel;
use serde_json::Value;

use crate::progress::{PhaseEvent, ProgressSink};

/// The field names a provider spells its finish reason with, in preference order.
///
/// Confirmed against the vendored rig-core provider sources (first 0.41, re-read at 0.42) rather than
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
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Read one response. Only the extraction survives; the raw payload stays on the
    /// response rig hands back (see the module doc on what we keep).
    ///
    /// A response built without a provider behind it carries `Value::Null` in `raw`
    /// (rig's documented "no provider seam"), which reads as "no finish reason".
    fn observe(response: &CompletionResponse) -> Self {
        let raw = &response.raw;
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
            finish_reason: finish_reason(raw),
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
/// on the signal built to carry none. A handful of classes is the right resolution for
/// "what kind of thing went wrong", and the message is still in the error the caller gets.
fn completion_error_type(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::Http(_) => "http_error",
        ProviderError::Json(_) => "json_error",
        ProviderError::Url(_) => "url_error",
        ProviderError::Request(_) => "request_error",
        ProviderError::Response(_) => "response_error",
        ProviderError::Provider(_) => "provider_error",
        ProviderError::ProviderResponse(_) => "provider_response_error",
        // `ProviderError` is `#[non_exhaustive]`: a variant rig adds later lands under
        // the one label below until someone names it, which keeps the series bounded.
        _ => "other_error",
    }
}

/// The same class label for an error that came back through rig's effect protocol as an
/// [`ErrorReport`], which is what an agent hook sees. `ErrorKind::code` is rig's own
/// stable machine name, one value per kind and never a message.
fn report_error_type(report: &ErrorReport) -> &'static str {
    report.kind.code()
}

/// The per-call slot a [`Watch`] records into: every completion of one phase, in
/// order. Cheaply cloneable and `Send + Sync`, because the model is cloned into each
/// agent rig builds (once per loop iteration, again for the forced final turn) and
/// every clone must land in the *same* log.
///
/// **Scoped to one call, never global.** A phase makes its own log and hands it to
/// its own wrapper, so two concurrent consults never see each other's turns. It has
/// nothing to do with the kaish kernel — the log holds plain data, crosses `.await`
/// freely, and never touches the `!Send` kernel behind `KaishWorker`.
#[derive(Clone, Default, Debug)]
pub struct CompletionLog {
    turns: Arc<Mutex<Vec<TurnRecord>>>,
    /// How many completions were *attempted*, which is not how many were recorded.
    ///
    /// A `TurnRecord` describes a provider *response*, so a call that failed produces
    /// none — deliberate, and pinned by `a_provider_error_passes_through_untouched`.
    /// That makes `turns` the wrong number for "how many inference calls did this
    /// phase make", which the GenAI conventions say must include the failures. So the
    /// attempt is counted separately, at the same seam, and the two answer different
    /// questions: `turns` is what the provider said, `attempts` is what kaibo asked.
    attempts: Arc<AtomicU64>,
}

impl CompletionLog {
    /// An empty log for one phase call.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many completions this phase attempted — every turn of the tool loop, the
    /// forced final turn, each retry (a malformed generation or a transient transport
    /// failure sent again), and every call that failed. The watcher sits inside the
    /// retry wrapper, so each attempt the provider sees is one here. This is
    /// `gen_ai.invoke_agent.inference_calls`.
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed)
    }

    fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// Every completion *response* recorded so far, oldest first — tool-loop turns
    /// included. A failed call is absent; see [`attempts`](Self::attempts).
    pub fn turns(&self) -> Vec<TurnRecord> {
        self.turns.lock().expect("completion log poisoned").clone()
    }

    /// The most recent completion — the turn that produced the answer (or failed to).
    pub fn last(&self) -> Option<TurnRecord> {
        self.turns
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

    /// How many completion *responses* this phase recorded. Not the inference-call
    /// count — a failed call is missing from it; use [`attempts`](Self::attempts).
    pub fn len(&self) -> usize {
        self.turns.lock().expect("completion log poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&self, turn: TurnRecord) {
        self.turns
            .lock()
            .expect("completion log poisoned")
            .push(turn);
    }
}

/// Observes every completion of one phase: times it, counts it, and records what the
/// response reported, then leaves the answer exactly as the provider gave it.
///
/// Two seams feed one recorder. Inside a tool loop the watcher is an [`AgentHook`]: rig's
/// agent dispatches each completion through its handler table, and the hook sees the
/// dispatch begin ([`AgentHook::on_dispatch`]) and the answer or failure come back
/// ([`AgentHook::on_outcome`]) for **every** turn of the loop, the forced final turn
/// included. A single-shot lane has no agent, so it calls [`Watch::call`], which brackets
/// one `DynModel` call the same way. Both write through [`Watch::record`].
///
/// A hook cannot change what it observes here: `on_outcome` always returns
/// [`OutcomeAction::Proceed`], so the response and every error reach the loop unchanged.
#[derive(Clone, Debug)]
pub struct Watch {
    log: CompletionLog,
    /// Who this arm calls, for the GenAI client metrics. `None` for an arm with no
    /// provider behind it — the offline scripted client — which records nothing; see
    /// [`crate::metrics::PhaseIdentity`].
    ident: Option<MetricIdent>,
    /// Where each call's duration goes as a progress beat, and which cast role to
    /// label it — `None` for a watcher nobody listens to. Separate from `ident` on
    /// purpose: a beat is liveness for the caller, so the offline scripted arm emits
    /// it too, while a metric is a provider fact that arm has no business recording.
    beat: Option<ChatBeat>,
    /// When each in-flight dispatch began, keyed by rig's effect id. Hook callbacks are
    /// `&self` and a turn's calls can overlap, so the map is shared and locked.
    started: Arc<Mutex<HashMap<EffectId, Instant>>>,
}

/// The progress half of a [`Watch`]: the sink a call's duration is emitted to and the
/// role it is labeled with.
#[derive(Clone, Debug)]
struct ChatBeat {
    sink: Arc<dyn ProgressSink>,
    agent: &'static str,
}

/// The owned half of [`crate::metrics::CallIdent`]. Owned because the watcher is cloned
/// into every agent a phase builds and must outlive the call site that named it.
#[derive(Clone, Debug)]
struct MetricIdent {
    provider: crate::credentials::ProviderKind,
    model: String,
}

impl Watch {
    /// A watcher recording into `log`, and — when the arm names a provider — into the
    /// client metrics.
    pub fn new(
        log: CompletionLog,
        ident: Option<crate::metrics::PhaseIdentity>,
        model_name: &str,
    ) -> Self {
        Self {
            log,
            ident: ident.map(|i| MetricIdent {
                provider: i.provider,
                model: model_name.to_string(),
            }),
            beat: None,
            started: Arc::default(),
        }
    }

    /// Emit a [`PhaseEvent::ChatCompleted`] into `sink` after every call, labeled with
    /// `agent` (the cast role — `"synth"` / `"explorer"`). The tool-loop phases attach
    /// this; the single-shot lanes leave it off.
    pub fn with_beat(mut self, sink: Arc<dyn ProgressSink>, agent: &'static str) -> Self {
        self.beat = Some(ChatBeat { sink, agent });
        self
    }

    /// The log this watcher writes.
    pub fn log(&self) -> &CompletionLog {
        &self.log
    }

    /// One finished call: `result` is the response, or the failure's `error.type`.
    fn record(&self, elapsed: Duration, result: Result<&CompletionResponse, &'static str>) {
        // Counted before the outcome is inspected, so a failed call is an inference
        // call — which is what the conventions require and what a phase that died at
        // turn 90 has to report to be read honestly.
        self.log.record_attempt();
        // The beat goes out before the outcome is inspected for the same reason: a call
        // that failed after three minutes is the latency a caller most wants to see.
        if let Some(beat) = &self.beat {
            beat.sink.emit(PhaseEvent::ChatCompleted {
                agent: beat.agent,
                elapsed,
            });
        }
        if let Some(ident) = &self.ident {
            let call = crate::metrics::CallIdent {
                provider: ident.provider,
                model: &ident.model,
            };
            match result {
                // A failed call has no usage to report, but its duration is the
                // number an operator most wants when a provider is degrading.
                Err(kind) => crate::metrics::record_completion(call, elapsed, None, Some(kind)),
                Ok(response) => {
                    crate::metrics::record_completion(call, elapsed, Some(&response.usage), None)
                }
            }
        }
        if let Ok(response) = result {
            self.log.push(TurnRecord::observe(response));
        }
    }

    /// One completion straight to `model`, bracketed and recorded — the single-shot
    /// lanes' seam. The response and every error come back untouched.
    pub async fn call(
        &self,
        model: &DynModel<Completion>,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let started = Instant::now();
        let result = model.call(request).await;
        let elapsed = started.elapsed();
        match &result {
            Ok(response) => self.record(elapsed, Ok(response)),
            Err(error) => self.record(elapsed, Err(completion_error_type(error))),
        }
        result
    }

    fn lock_started(&self) -> std::sync::MutexGuard<'_, HashMap<EffectId, Instant>> {
        self.started.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl AgentHook for Watch {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if matches!(event.kind, EffectKind::Completion { .. }) {
            self.lock_started().insert(event.id, Instant::now());
        }
        DispatchAction::Proceed
    }

    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if !matches!(event.kind, EffectKind::Completion { .. }) {
            return OutcomeAction::Proceed;
        }
        let elapsed = self
            .lock_started()
            .remove(&event.id)
            .map(|at| at.elapsed())
            .unwrap_or_default();
        match event.outcome {
            Ok(Answered::Completion(response)) => self.record(elapsed, Ok(response)),
            Ok(_) => {}
            Err(report) => self.record(elapsed, Err(report_error_type(report))),
        }
        OutcomeAction::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consult::Llm;
    use crate::test_support::{
        provider_error, response_error, text_response, tool_call_response, usage, with_raw,
        with_usage, ScriptedClient,
    };
    use serde_json::json;

    /// The beat is the one place a caller learns how long a call took, so it must carry
    /// the wall time the call actually spent and the role it was labeled with — and it
    /// must fire for a failed call too, since a call that died after minutes is the
    /// latency a caller most wants to see.
    #[tokio::test]
    async fn the_beat_carries_the_calls_wall_time_and_role_and_fires_on_failure() {
        use crate::progress::PhaseEvent;
        use crate::test_support::RecordingSink;
        let sink = Arc::new(RecordingSink::default());
        let model = model(|req| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            // The second call fails: `max_tokens` is the switch.
            if req.max_tokens == Some(1) {
                Err(provider_error("boom"))
            } else {
                Ok(text_response("hi"))
            }
        });
        let watch = Watch::new(CompletionLog::new(), None, "m").with_beat(sink.clone(), "synth");

        watch.call(model.dyn_model(), req()).await.expect("first call answers");
        let mut failing = req();
        failing.max_tokens = Some(1);
        watch
            .call(model.dyn_model(), failing)
            .await
            .expect_err("second call fails");

        let events = sink.events();
        assert_eq!(
            events.len(),
            2,
            "one beat per call, failed or not: {events:?}"
        );
        for event in &events {
            match event {
                PhaseEvent::ChatCompleted { agent, elapsed } => {
                    assert_eq!(*agent, "synth");
                    assert!(
                        *elapsed >= std::time::Duration::from_millis(30),
                        "the beat carries the call's wall time, got {elapsed:?}"
                    );
                }
                other => panic!("only chat beats expected, got {other:?}"),
            }
        }
    }

    /// A malformed generation sent again beats twice — one per attempt, each timed on
    /// its own — and the attempt counter says two, while one response is recorded. Neither
    /// number ever includes a wait between attempts.
    #[tokio::test]
    async fn a_retried_turn_beats_and_counts_once_per_attempt() {
        use crate::completion_retry::retry_malformed;
        use crate::progress::PhaseEvent;
        use crate::test_support::RecordingSink;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sink = Arc::new(RecordingSink::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let model = model(move |_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(provider_error("MALFORMED_FUNCTION_CALL"))
            } else {
                Ok(text_response("hi"))
            }
        });
        let log = CompletionLog::new();
        let watch = Watch::new(log.clone(), None, "m").with_beat(sink.clone(), "synth");

        retry_malformed("m", || watch.call(model.dyn_model(), req()))
            .await
            .expect("the retry answers");

        assert_eq!(calls.load(Ordering::SeqCst), 2, "one fumble, one answer");
        assert_eq!(log.attempts(), 2, "the counter sees each attempt");
        assert_eq!(log.turns().len(), 1, "one response was recorded");
        let elapsed: Vec<_> = sink
            .events()
            .into_iter()
            .map(|e| match e {
                PhaseEvent::ChatCompleted {
                    agent: "synth",
                    elapsed,
                } => elapsed,
                other => panic!("only synth chat beats expected, got {other:?}"),
            })
            .collect();
        assert_eq!(elapsed.len(), 2, "one beat per attempt");
        for e in elapsed {
            assert!(
                e >= std::time::Duration::from_millis(20) && e < std::time::Duration::from_secs(1),
                "each beat is one attempt's own time, got {e:?}"
            );
        }
    }

    /// A watcher without a beat — the single-shot lanes' shape — emits nothing.
    #[tokio::test]
    async fn a_watcher_without_a_beat_emits_nothing() {
        use crate::test_support::RecordingSink;
        let sink = Arc::new(RecordingSink::default());
        let model = model(|_| Ok(text_response("hi")));
        let watch = Watch::new(CompletionLog::new(), None, "m");
        watch.call(model.dyn_model(), req()).await.expect("answers");
        assert!(sink.events().is_empty());
    }

    /// The hook seam: inside a real agent loop the watcher sees every completion the
    /// loop makes, counts it, times it, and records what the response reported — with no
    /// help from the loop's own code. A tool-call turn then a final answer is two
    /// completions, two beats, two records.
    #[tokio::test]
    async fn the_hook_records_every_turn_of_a_real_agent_loop() {
        use crate::progress::PhaseEvent;
        use crate::test_support::{is_finalize_turn, RecordingSink};
        let sink = Arc::new(RecordingSink::default());
        let model = model(|req| {
            if is_finalize_turn(req) || crate::test_support::transcript_text(req).contains("exit:")
            {
                Ok(with_raw(text_response("done"), openai_raw("stop")))
            } else {
                Ok(with_raw(
                    tool_call_response("c1", "run_kaish", json!({"script": "echo hi"})),
                    openai_raw("tool_calls"),
                ))
            }
        });
        let log = CompletionLog::new();
        let watch = Watch::new(log.clone(), None, "m").with_beat(sink.clone(), "explorer");
        let dir = tempfile::tempdir().expect("tempdir");
        let worker = crate::sandbox::KaishWorker::spawn(dir.path()).expect("kaish worker");
        let agent = rig_agent::AgentBuilder::new(model_inner(&model))
            .tool(crate::explorer::RunKaish::new(worker))
            .build();
        let response = agent
            .prompt("go")
            .add_hook(watch)
            .max_turns(4)
            .await
            .expect("the loop answers");
        assert_eq!(response.output(), "done");
        assert_eq!(log.attempts(), 2, "two completions attempted");
        let turns = log.turns();
        assert_eq!(turns.len(), 2, "both responses recorded: {turns:?}");
        assert_eq!(turns[0].tool_calls, 1);
        assert_eq!(turns[1].text_chars, 4);
        let beats = sink
            .events()
            .into_iter()
            .filter(|e| matches!(e, PhaseEvent::ChatCompleted { agent: "explorer", .. }))
            .count();
        assert_eq!(beats, 2, "one beat per completion");
    }

    /// A one-turn request, the shape rig's agent builds for a toolless call.
    fn req() -> CompletionRequest {
        CompletionRequest::new(rig_core::message::Message::user("q")).max_tokens(64)
    }

    /// The model inside an [`Llm`], for builders that take rig's erased type directly.
    fn model_inner(llm: &Llm) -> DynModel<Completion> {
        llm.dyn_model().clone()
    }

    /// Each class of rig's error enum maps to its own `error.type` label. `ProviderError`
    /// is `#[non_exhaustive]`, so a variant rig adds later falls under `other_error` until
    /// someone names it; this pins the names the existing classes carry.
    #[test]
    fn every_provider_error_class_has_its_own_error_type() {
        use crate::test_support::status_error;
        let json = serde_json::from_str::<Value>("{").unwrap_err();
        let url = reqwest::Url::parse("not a url").unwrap_err();
        let cases: Vec<(ProviderError, &str)> = vec![
            (
                ProviderError::Http(Arc::new(rig_core::http_client::Error::StreamEnded)),
                "http_error",
            ),
            (ProviderError::Json(Arc::new(json)), "json_error"),
            (ProviderError::Url(url), "url_error"),
            (
                ProviderError::request(std::io::Error::other("x")),
                "request_error",
            ),
            (response_error("x"), "response_error"),
            (provider_error("x"), "provider_error"),
            // A non-success status is a preserved provider reply, not a transport error.
            (status_error(503, "overloaded"), "provider_response_error"),
        ];
        for (error, want) in cases {
            assert_eq!(completion_error_type(&error), want, "{error:?}");
        }
        // An agent hook sees the same failure as an `ErrorReport`; the label is rig's own
        // stable code for its kind.
        assert_eq!(
            report_error_type(&status_error(503, "overloaded").report()),
            "provider_response"
        );
    }

    /// A scripted model whose every call answers with `respond`.
    fn model<F>(respond: F) -> Llm
    where
        F: Fn(&CompletionRequest) -> Result<CompletionResponse, ProviderError>
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

    /// Drive the watcher the way the single-shot lanes do — one call — and prove it
    /// changes nothing observable: same content, same usage, same raw response. Only the
    /// log is new.
    #[tokio::test]
    async fn the_watcher_is_a_pure_passthrough() {
        let model = model(|_req| {
            Ok(with_raw(
                with_usage(text_response("the answer"), usage(11, 7)),
                openai_raw("stop"),
            ))
        });

        let bare = model
            .dyn_model()
            .call(req())
            .await
            .expect("bare completion");
        let log = CompletionLog::new();
        let watched = Watch::new(log.clone(), None, "scripted")
            .call(model.dyn_model(), req())
            .await
            .expect("watched completion");

        assert_eq!(
            serde_json::to_value(&watched.choice).unwrap(),
            serde_json::to_value(&bare.choice).unwrap(),
            "the watcher must not touch the content"
        );
        assert_eq!(watched.usage, bare.usage, "usage passes through");
        assert_eq!(
            watched.raw, bare.raw,
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

        let bare = model.dyn_model().call(req()).await;
        let watched = Watch::new(log.clone(), None, "scripted")
            .call(model.dyn_model(), req())
            .await;

        assert_eq!(
            format!("{:?}", watched.unwrap_err()),
            format!("{:?}", bare.unwrap_err()),
            "the provider's error reaches the caller unchanged"
        );
        assert!(log.is_empty(), "a failed call records no turn");
        assert_eq!(
            log.attempts(),
            1,
            "but it IS an inference call — the conventions count failures, and a phase \
             that died at turn 90 must report 90, not 89"
        );
    }

    /// A tool-call turn is recorded too — no text, one call, and whatever the
    /// provider said about stopping. The loop's inner turns are all this shape.
    #[tokio::test]
    async fn a_tool_call_turn_is_recorded_with_no_text() {
        let model = model(|_req| {
            Ok(with_raw(
                tool_call_response("c1", "run_kaish", json!({"script": "ls"})),
                anthropic_raw(json!("tool_use")),
            ))
        });
        let log = CompletionLog::new();
        Watch::new(log.clone(), None, "scripted")
            .call(model.dyn_model(), req())
            .await
            .expect("completion");

        let turn = log.last().expect("one turn");
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(turn.text_chars, 0);
        assert_eq!(turn.tool_calls, 1);
    }
}
