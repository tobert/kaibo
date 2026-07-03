//! A scripted, offline stand-in for a provider client.
//!
//! kaibo's whole model surface flows through rig's [`CompletionClient`] /
//! [`CompletionModel`] traits ([`crate::consult::run_phase`] is generic over the
//! client). The live tests exercise that loop against a real provider; this harness
//! lets us drive the *same* loop deterministically, with no network — so we can pin
//! behavior the live tests can only observe flakily: that a `consult` tool call
//! actually drives the nested `explore′` agent and aggregates into the report, that
//! the turn-cap finalize path produces an answer, that a session's prior turns reach
//! the next prompt.
//!
//! ## Design — content-driven, not consumption-ordered
//!
//! A real model sees the *whole* [`CompletionRequest`] on every call and decides
//! from it. The mock mirrors that: a **responder** is `Fn(&CompletionRequest) ->
//! Result<…>`, registered per model id, that branches on the request's content
//! (preamble, chat history, `tool_choice`, …) rather than on "which call number is
//! this". That matters because rig executes a turn's tool calls with
//! `buffer_unordered` (default concurrency 1, but *unordered* by construction): a
//! queue-pop mock ("the Nth call returns the Nth step") would be correct today and
//! race the day a driver turn emits two `explore` calls or someone bumps tool
//! concurrency. Branching on content is immune to that — and it also handles the
//! finalize replay for free (`tool_choice == None` ⇒ "answer now"), since the mock
//! reads the same signal the real model would.
//!
//! Two separable concerns, two primitives:
//! - **Response strategy** — the per-model responders below.
//! - **Request log** — every inbound request is snapshotted into [`ScriptedClient`]'s
//!   log so a test can assert *what was asked* (which model saw which preamble, what
//!   `max_tokens`/`additional_params` rode along) independently of how it answered.
//!
//! `stream()` is intentionally `unimplemented!()`: kaibo only ever drives the
//! non-streaming `prompt` loop, which calls `completion()`. If kaibo ever adopts the
//! streaming prompt path, this mock grows a `stream` arm — a known boundary, not a
//! silent gap.

#![allow(dead_code)] // shared across test binaries; each uses only a subset.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    AssistantContent, Message, ToolChoice, ToolResultContent, UserContent,
};
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage, Usage,
};
use rig_core::OneOrMany;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How the mock answers one request for one model: branch on the request's content
/// and return a response, or an error to exercise the failure paths.
pub type Responder = Arc<
    dyn Fn(&CompletionRequest) -> Result<CompletionResponse<()>, CompletionError> + Send + Sync,
>;

/// A snapshot of one inbound completion request, captured for post-hoc assertions.
/// Decoupled from the responder so a test can assert *what was asked* separately
/// from *how it was answered*.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The model id the loop addressed (the routing key — explorer vs. synth).
    pub model: String,
    /// The agent preamble (role framing) as rig forwarded it.
    pub preamble: Option<String>,
    /// Names of the tools declared on this request, e.g. `["run_kaish", "explore"]`.
    pub tool_names: Vec<String>,
    /// Concatenated text of every `User` message in the history (oldest→newest),
    /// joined by newlines — enough to assert a prior turn's Q&A reached the prompt.
    pub user_text: String,
    /// Like `user_text` but *also* including tool-result text — what the model was
    /// actually shown. Use this to assert a prior tool call's output (a `run_kaish`
    /// result, an `explore′` report) survived into a later turn, e.g. that the forced
    /// finalize turn carries the partial work rather than a blank history.
    pub transcript: String,
    /// Provider-specific params (e.g. the thinking toggle) forwarded by `run_phase`.
    pub additional_params: Option<Value>,
    pub max_tokens: Option<u64>,
    /// `Some(ToolChoice::None)` marks the forced finalize turn after a turn-cap hit.
    pub tool_choice: Option<ToolChoice>,
}

impl RecordedRequest {
    fn capture(model: &str, req: &CompletionRequest) -> Self {
        Self {
            model: model.to_string(),
            preamble: preamble(req),
            tool_names: req.tools.iter().map(|t| t.name.clone()).collect(),
            user_text: user_text(req),
            transcript: transcript_text(req),
            additional_params: req.additional_params.clone(),
            max_tokens: req.max_tokens,
            tool_choice: req.tool_choice.clone(),
        }
    }
}

/// A scripted provider client. Cheap to clone — clones share one responder table and
/// one request log, so the client rig hands to the nested `explore′` agent (a clone)
/// records into the same log and routes through the same responders.
#[derive(Clone)]
pub struct ScriptedClient {
    responders: Arc<HashMap<String, Responder>>,
    /// Model ids whose `completion()` never returns — a *wedged provider*: the call
    /// parks on an effectively unbounded async sleep instead of answering. This is
    /// the failure mode a stopped/hung local server produces (the request goes out,
    /// no response ever comes), and it's what the call-deadline backstop must catch.
    hangers: Arc<HashSet<String>>,
    log: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl ScriptedClient {
    pub fn builder() -> ScriptedBuilder {
        ScriptedBuilder {
            responders: HashMap::new(),
            hangers: HashSet::new(),
        }
    }

    /// Snapshot of every request seen so far, in call order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.log.lock().expect("request log poisoned").clone()
    }

    /// Requests addressed to `model`, in call order.
    pub fn requests_for(&self, model: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.model == model)
            .collect()
    }
}

pub struct ScriptedBuilder {
    responders: HashMap<String, Responder>,
    hangers: HashSet<String>,
}

impl ScriptedBuilder {
    /// Register how the model named `id` answers. The closure sees the whole request
    /// and returns a response (or an error, to drive a failure path).
    pub fn on_model<F>(mut self, id: impl Into<String>, responder: F) -> Self
    where
        F: Fn(&CompletionRequest) -> Result<CompletionResponse<()>, CompletionError>
            + Send
            + Sync
            + 'static,
    {
        self.responders.insert(id.into(), Arc::new(responder));
        self
    }

    /// Make model `id` a *wedged provider*: its `completion()` records the request
    /// then parks forever (an unbounded async sleep), never answering — the
    /// stopped/hung-backend shape the call-deadline test drives. Needs no responder.
    pub fn hang_model(mut self, id: impl Into<String>) -> Self {
        self.hangers.insert(id.into());
        self
    }

    pub fn build(self) -> ScriptedClient {
        ScriptedClient {
            responders: Arc::new(self.responders),
            hangers: Arc::new(self.hangers),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// The model handle rig builds per `client.agent(model)`. Carries its own id (the
/// routing key) plus shared handles to the responder table and request log.
#[derive(Clone)]
pub struct ScriptedModel {
    id: String,
    responders: Arc<HashMap<String, Responder>>,
    hangers: Arc<HashSet<String>>,
    log: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// Streaming response placeholder: kaibo never streams, so this is never built. It
/// exists only to satisfy `CompletionModel::StreamingResponse`'s bounds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoStream;

impl GetTokenUsage for NoStream {
    fn token_usage(&self) -> Option<Usage> {
        None
    }
}

impl CompletionClient for ScriptedClient {
    type CompletionModel = ScriptedModel;
}

impl CompletionModel for ScriptedModel {
    type Response = ();
    type StreamingResponse = NoStream;
    type Client = ScriptedClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            id: model.into(),
            responders: client.responders.clone(),
            hangers: client.hangers.clone(),
            log: client.log.clone(),
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Record first, so even a request that the responder errors on is observable.
        self.log
            .lock()
            .expect("request log poisoned")
            .push(RecordedRequest::capture(&self.id, &request));
        // A wedged provider: the request went out (recorded above) and no answer ever
        // comes. Park effectively forever — a real call-deadline fires long before this
        // elapses, and the request is already observable in the log.
        if self.hangers.contains(&self.id) {
            tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
        }
        let responder = self.responders.get(&self.id).unwrap_or_else(|| {
            panic!(
                "scripted client has no responder for model {:?} (registered: {:?})",
                self.id,
                self.responders.keys().collect::<Vec<_>>()
            )
        });
        responder(&request)
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<
        rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
        CompletionError,
    > {
        unimplemented!("kaibo drives the non-streaming prompt loop; the mock never streams")
    }
}

// ---- response builders -----------------------------------------------------

/// A final text answer — ends the tool loop.
pub fn text_response(text: impl Into<String>) -> CompletionResponse<()> {
    response(OneOrMany::one(AssistantContent::text(text)))
}

/// A single tool call — drives one more loop turn.
pub fn tool_call_response(
    id: impl Into<String>,
    name: impl Into<String>,
    args: Value,
) -> CompletionResponse<()> {
    response(OneOrMany::one(AssistantContent::tool_call(id, name, args)))
}

/// Several tool calls in *one* assistant turn — the co-tool-call case (e.g. a turn
/// that calls `view_image` alongside `run_kaish`). rig runs them together and folds
/// all their results into a single user turn, which is exactly the shape the
/// view_image turn-boundary break must tolerate without orphaning a `tool_use`.
pub fn tool_calls_response(calls: Vec<(&str, &str, Value)>) -> CompletionResponse<()> {
    let contents: Vec<AssistantContent> = calls
        .into_iter()
        .map(|(id, name, args)| AssistantContent::tool_call(id, name, args))
        .collect();
    response(OneOrMany::many(contents).expect("at least one tool call"))
}

fn response(choice: OneOrMany<AssistantContent>) -> CompletionResponse<()> {
    CompletionResponse {
        choice,
        usage: Usage::new(),
        raw_response: (),
        message_id: None,
    }
}

/// A provider-side error — drives `run_phase`'s failure arm (the model loop fails
/// before it concludes). Return `Err(provider_error(..))` from a responder.
pub fn provider_error(msg: impl Into<String>) -> CompletionError {
    CompletionError::ProviderError(msg.into())
}

// ---- request accessors -----------------------------------------------------

/// Concatenated text of every `User` message in the request history, oldest→newest,
/// joined by newlines. Robust for asserting a substring (a prior turn's Q&A, the
/// finalize note) is present *somewhere* in what the model was shown.
pub fn user_text(req: &CompletionRequest) -> String {
    req.chat_history
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The agent's role framing, wherever rig put it: the legacy `preamble` field if set,
/// else a leading `System` message in the history (rig 0.34 prefers the latter).
pub fn preamble(req: &CompletionRequest) -> Option<String> {
    if let Some(p) = &req.preamble {
        return Some(p.clone());
    }
    req.chat_history.iter().find_map(|m| match m {
        Message::System { content } => Some(content.clone()),
        _ => None,
    })
}

/// Everything the model was shown in user turns — `User` text *and* tool-result
/// text — oldest→newest, joined by newlines. Use this to detect that a prior tool
/// call's output (an `explore′` report, a `run_kaish` result) has come back into the
/// loop, since tool results arrive as `UserContent::ToolResult`, not plain text.
pub fn transcript_text(req: &CompletionRequest) -> String {
    req.chat_history
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.clone()),
                        UserContent::ToolResult(tr) => Some(
                            tr.content
                                .iter()
                                .filter_map(|rc| match rc {
                                    ToolResultContent::Text(t) => Some(t.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True if the request declares a tool named `name`.
pub fn has_tool(req: &CompletionRequest, name: &str) -> bool {
    req.tools.iter().any(|t| t.name == name)
}

/// True on the forced finalize turn (`run_phase` sets `ToolChoice::None` after the
/// turn cap, forbidding further tool calls).
pub fn is_finalize_turn(req: &CompletionRequest) -> bool {
    req.tool_choice == Some(ToolChoice::None)
}

/// A [`ProgressSink`](crate::progress::ProgressSink) that records every event, so a
/// loop test can prove the deep loop (a delegated sweep, a direct `run_kaish` read)
/// actually emitted progress. The production sink renders these onto the MCP wire;
/// here we just capture them and assert.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<crate::progress::PhaseEvent>>,
}

impl RecordingSink {
    /// A snapshot of the events emitted so far, in order.
    pub fn events(&self) -> Vec<crate::progress::PhaseEvent> {
        self.events.lock().expect("recording sink poisoned").clone()
    }
}

impl crate::progress::ProgressSink for RecordingSink {
    fn emit(&self, event: crate::progress::PhaseEvent) {
        self.events
            .lock()
            .expect("recording sink poisoned")
            .push(event);
    }
}
