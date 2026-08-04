//! `kaibo acp` — the Agent Client Protocol front door.
//!
//! kaibo's second protocol front door, alongside MCP (`src/server/`) and the CLI
//! (`src/cli.rs`): an ACP v1 agent, so an ACP client (Zed, Toad, JetBrains) drives
//! kaibo the same way an MCP client does, over stdio. `session/prompt` now runs the
//! real consult loop: it resolves the session's current cast to arms the same way
//! the MCP `consult` tool does (`Arm::from_slot`, the single live construction
//! point), drives `consult` (`src/consult/`), and streams progress as ACP
//! `session/update` notifications while it works.
//!
//! ACP's session concept doesn't map onto kaibo's `consult` session (a lean
//! question/answer history replayed as context, `src/session.rs`) or the durable
//! store (`src/store.rs`): an ACP session is a whole client conversation — a cwd,
//! an MCP server list, a mode — that a prompt turn runs inside. This chunk keeps
//! that mapping in a small in-memory table, keyed by a `session-N` id (the `job-N`
//! style already used in `src/jobs.rs`), and reuses the ACP session id itself as the
//! key into kaibo's own in-memory multi-turn `Sessions` store — so successive
//! prompts in one ACP session replay as one `consult` thread, through the real
//! `consult_session_turn` replay/record machinery, not a parallel one.
//!
//! # Progress → `session/update`
//!
//! [`AcpProgressSink`] renders each [`crate::progress::PhaseEvent`] as a
//! `session/update` notification: kaibo's granularity is per-milestone (a sweep
//! started, a kaish script ran), not token-streamed, so each beat becomes one
//! notification. `SweepStarted`/`SweepFinished` open and close one ACP `ToolCall`
//! (they are the one pair kaibo's engine already emits 1:1); `Attached` updates that
//! same open call; `KaishRun` opens its own `ToolCall` that kaibo has no matching
//! "finished" beat for today, so it is announced `InProgress` and never explicitly
//! closed — reporting only what kaibo actually knows, not a fabricated completion.
//! The bookending `PhaseStarted`/`PhaseFinished`/`TurnCapReached` beats are meta
//! narration about the whole turn, not a tool call, so they render as
//! `AgentThoughtChunk` text (kaibo's own `PhaseEvent::message()`, the same one-liner
//! the MCP progress notifications and the CLI's `TerminalSink` show).
//!
//! # Cancellation
//!
//! ACP's `session/cancel` is its own domain notification (`CancelNotification`),
//! distinct from the SDK's low-level JSON-RPC `$/cancel_request` (a different
//! mechanism, for cancelling one outstanding request by id — see
//! `agent_client_protocol::concepts::cancellation`). kaibo tracks the in-flight
//! turn's `tokio::task::AbortHandle` on the session record — the same
//! spawn-then-`AbortHandle` shape `src/jobs.rs` uses for `job_cancel` — so
//! `session/cancel` aborts it directly; a session with nothing running is a clean
//! no-op. `session/prompt` itself spawns the actual consult work and returns from
//! its handler immediately (mirroring the SDK's own documented cancellation
//! pattern): the connection's event loop can't process a new message while a
//! handler's future is still running, so awaiting the consult call *inline* would
//! make `session/cancel` unreachable until the turn already finished.
//!
//! # Non-text prompt content
//!
//! `session/prompt` accepts only `ContentBlock::Text` blocks in this build. An
//! image/audio/resource block is refused with a clear error naming the block kind:
//! consult's vision plumbing (`view_image`, `ConsultAttachment::Image`) expects a
//! file path under the project root, not inline bytes over the wire, so accepting
//! one honestly would need a real hand-off (spooling the bytes somewhere kaish can
//! read them) that this chunk does not build. Text-only is the honest baseline ACP
//! itself requires every agent to support.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionMode, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Error};

use crate::config::{Cast, Config, ModelRole};
use crate::consult::{consult, Arm, ConsultConfig, ExploreConfig, PhaseContext};
use crate::progress::{PhaseEvent, ProgressSink};
use crate::server::{append_warnings, consultation_failure_text, with_provenance, Resolver};
use crate::session::Sessions;

/// One ACP session this process has minted: the client's working directory (the
/// `consult` root every turn runs against), which cast the session is currently
/// pinned to, and the in-flight turn's abort handle, if one is running.
#[derive(Debug, Clone)]
struct SessionRecord {
    cwd: PathBuf,
    mode_id: String,
    /// What `session/cancel` aborts. `None` between turns and while idle — a cancel
    /// on an idle session is a clean no-op.
    running: Option<tokio::task::AbortHandle>,
}

/// How `session/prompt` builds this turn's explorer/synth [`Arm`]s for a resolved
/// cast. The live path threads through the shared [`Resolver`] (`Arm::from_slot` —
/// real backends, keys, HTTP clients, the single live construction point every
/// other front door uses); tests substitute arms built directly over a
/// `ScriptedClient` via `Arm::new` — the exact injection seam `consult`'s own
/// offline tests use (`src/consult/engine.rs`'s test module), not a parallel
/// harness. Keyed by cast name, so a `session/set_mode` switch resolves a
/// different scripted pair.
enum ArmSource {
    Live,
    #[cfg(test)]
    Scripted(HashMap<String, (Arm, Arm)>),
}

/// Shared state behind every ACP connection this process serves: the resolved
/// config (via the same [`Resolver`] the MCP handler and CLI use), the in-memory
/// session table, and the multi-turn `consult` session store the ACP session id
/// itself keys into.
#[derive(Clone)]
pub struct AcpAgentState {
    inner: Arc<Inner>,
}

struct Inner {
    resolver: Resolver,
    /// Configured cast names, in `Config::casts`'s `BTreeMap` order — deterministic,
    /// so the advertised mode list doesn't reorder between calls.
    cast_names: Vec<String>,
    default_cast: String,
    /// Multi-turn `consult` history, keyed by the ACP session id string — an
    /// in-memory backend (today's only ACP option; the durable store is a later
    /// chunk's call, same as it was for MCP before persistence shipped).
    consult_sessions: Sessions,
    sessions: Mutex<HashMap<SessionId, SessionRecord>>,
    next_session: AtomicU64,
    arms: ArmSource,
}

impl AcpAgentState {
    /// Build the shared state from a resolved [`Config`]: one session mode per
    /// configured cast, starting on the config's default cast, arms resolved live
    /// through a [`Resolver`] built the same way every other front door builds one.
    /// Fallible for the same reason `Resolver::from_config` is everywhere else — a
    /// nonexistent `--root`/`--allow-path` is a loud construction error, not a
    /// silently-empty boundary.
    pub fn new(config: Arc<Config>) -> anyhow::Result<Self> {
        let resolver = Resolver::from_config(config)?;
        Ok(Self::build(resolver, ArmSource::Live))
    }

    /// Test-only constructor: a real [`Resolver`] over `config` (so cast metadata,
    /// prompts, sandbox, and session capacity are all the genuine article), but
    /// `session/prompt`'s arms come from `arms` (cast name → (explorer, synth))
    /// instead of `Resolver::arm`'s real backend construction — which can only ever
    /// build a networked client, never a `ScriptedClient`. Mirrors how `consult`'s
    /// own tests hand a `ScriptedClient` through `Arm::new`; no parallel harness.
    #[cfg(test)]
    pub(crate) fn new_scripted(config: &Config, arms: HashMap<String, (Arm, Arm)>) -> Self {
        let resolver = Resolver::from_config(Arc::new(config.clone()))
            .expect("test config resolves (no --root/--allow-path to canonicalize)");
        Self::build(resolver, ArmSource::Scripted(arms))
    }

    fn build(resolver: Resolver, arms: ArmSource) -> Self {
        let cast_names = resolver.config.casts.keys().cloned().collect();
        let default_cast = resolver.config.default_cast.clone();
        let consult_sessions = Sessions::Memory(crate::session::SessionStore::new(
            resolver.config.defaults.session_capacity,
        ));
        Self {
            inner: Arc::new(Inner {
                resolver,
                cast_names,
                default_cast,
                consult_sessions,
                sessions: Mutex::new(HashMap::new()),
                next_session: AtomicU64::new(1),
                arms,
            }),
        }
    }

    /// The session modes to advertise: one per configured cast, mode id == cast name.
    fn session_modes(&self) -> Vec<SessionMode> {
        self.inner
            .cast_names
            .iter()
            .map(|name| SessionMode::new(name.clone(), name.clone()))
            .collect()
    }

    /// Whether `mode_id` names a configured cast.
    fn known_mode(&self, mode_id: &str) -> bool {
        self.inner.cast_names.iter().any(|name| name == mode_id)
    }

    /// Mint the next `session-N` id — same style as the `job-N` ids in `src/jobs.rs`,
    /// not a UUID: no new dependency for a value only this process's ACP connections
    /// ever read back.
    fn mint_session_id(&self) -> SessionId {
        let n = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        SessionId::new(format!("session-{n}"))
    }

    /// Record a freshly created session, starting it on the default cast.
    fn insert_session(&self, id: SessionId, cwd: PathBuf) {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned");
        sessions.insert(
            id,
            SessionRecord {
                cwd,
                mode_id: self.inner.default_cast.clone(),
                running: None,
            },
        );
    }

    /// Snapshot a session's record, if it exists.
    fn session(&self, id: &SessionId) -> Option<SessionRecord> {
        self.inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned")
            .get(id)
            .cloned()
    }

    /// Move a known session onto `mode_id`. Returns `false` if the session is unknown
    /// (a caller-visible error, not this scaffold's problem to swallow).
    fn set_mode(&self, id: &SessionId, mode_id: String) -> bool {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned");
        match sessions.get_mut(id) {
            Some(record) => {
                record.mode_id = mode_id;
                true
            }
            None => false,
        }
    }

    /// Record the in-flight turn's abort handle, so a later `session/cancel` can
    /// find it. A no-op if the session vanished between the prompt starting and
    /// this call (can't happen today — sessions are never evicted — but harmless).
    fn set_running(&self, id: &SessionId, handle: tokio::task::AbortHandle) {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned");
        if let Some(record) = sessions.get_mut(id) {
            record.running = Some(handle);
        }
    }

    /// Clear a finished turn's abort handle so a stale one can't be aborted later.
    fn clear_running(&self, id: &SessionId) {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned");
        if let Some(record) = sessions.get_mut(id) {
            record.running = None;
        }
    }

    /// `session/cancel`: abort the session's in-flight prompt task, if any —
    /// cooperative and best-effort, matching the spec's own framing. Returns
    /// whether the session was known at all, so the notification handler can still
    /// log a truly unknown id without treating "known but idle" as one.
    fn cancel_running(&self, id: &SessionId) -> bool {
        let sessions = self
            .inner
            .sessions
            .lock()
            .expect("acp sessions mutex poisoned");
        match sessions.get(id) {
            Some(record) => {
                if let Some(handle) = &record.running {
                    handle.abort();
                }
                true
            }
            None => false,
        }
    }

    /// Resolve `mode_id` to a cast and refuse one whose synth runs on an offline
    /// lane — the same gate the MCP `consult` tool applies (`reject_offline_cast`):
    /// an ACP `session/prompt` turn is interactive by construction.
    fn resolve_cast(&self, mode_id: &str) -> Result<Cast, String> {
        let cast = self
            .inner
            .resolver
            .resolve_cast(Some(mode_id.to_string()))
            .map_err(|e| e.message.to_string())?;
        self.inner
            .resolver
            .reject_offline_cast(&cast, "session/prompt")
            .map_err(|e| e.message.to_string())?;
        Ok(cast)
    }

    /// Resolve one of `cast`'s slots into a live [`Arm`] — through the real
    /// [`Resolver`] in production, or the scripted map in tests. See [`ArmSource`].
    fn arm(&self, cast: &Cast, role: ModelRole) -> Result<Arm, String> {
        match &self.inner.arms {
            ArmSource::Live => self
                .inner
                .resolver
                .arm(cast, role)
                .map_err(|e| e.message.to_string()),
            #[cfg(test)]
            ArmSource::Scripted(map) => {
                let (explorer, synth) = map.get(&cast.name).ok_or_else(|| {
                    format!(
                        "no scripted arms registered for cast {:?} (registered: {:?})",
                        cast.name,
                        map.keys().collect::<Vec<_>>()
                    )
                })?;
                Ok(match role {
                    ModelRole::Explorer => explorer.clone(),
                    ModelRole::Synth => synth.clone(),
                    ModelRole::Image => {
                        return Err(format!(
                            "cast {:?}: session/prompt has no image role to resolve",
                            cast.name
                        ))
                    }
                })
            }
        }
    }
}

/// Extract the plain-text prompt from a `session/prompt`'s content blocks — text
/// blocks only in this build (see the module doc's "Non-text prompt content"
/// section). Multiple text blocks join with a blank line, matching how the MCP
/// `consult` tool folds multi-part input into one question string.
fn extract_prompt_text(blocks: &[ContentBlock]) -> Result<String, String> {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            other => {
                return Err(format!(
                    "session/prompt only accepts text content blocks in this build (got \
                     {}) — image/audio/resource input isn't wired into consult's vision \
                     plumbing yet. Send plain text.",
                    content_block_kind(other)
                ))
            }
        }
    }
    if parts.iter().all(|p| p.trim().is_empty()) {
        return Err(
            "empty prompt — session/prompt needs at least one non-empty text block".to_string(),
        );
    }
    Ok(parts.join("\n\n"))
}

/// A short name for a [`ContentBlock`] variant, for the non-text refusal message.
/// `ContentBlock` is `#[non_exhaustive]`, so this must carry a wildcard arm even
/// though every variant is named today.
fn content_block_kind(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource_link",
        ContentBlock::Resource(_) => "resource",
        _ => "unknown",
    }
}

/// Renders kaibo's [`PhaseEvent`] beats as ACP `session/update` notifications. See
/// the module doc's "Progress → `session/update`" section for the mapping and why
/// each event lands where it does. Fire-and-forget (`ProgressSink`'s contract): a
/// send failure (client gone, connection tearing down) is dropped rather than
/// allowed to fail the turn — the final `PromptResponse` is the authoritative
/// outcome either way.
struct AcpProgressSink {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    next_tool_id: AtomicU64,
    /// The most recently opened sweep's tool-call id, so `SweepFinished`/`Attached`
    /// update the SAME call instead of opening a new one. `consult` runs its
    /// delegated sweeps one at a time today, so a single slot (not a stack) pairs
    /// them correctly.
    current_sweep: Mutex<Option<ToolCallId>>,
}

impl AcpProgressSink {
    fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self {
            cx,
            session_id,
            next_tool_id: AtomicU64::new(1),
            current_sweep: Mutex::new(None),
        }
    }

    fn mint_tool_id(&self) -> ToolCallId {
        let n = self.next_tool_id.fetch_add(1, Ordering::Relaxed);
        ToolCallId::new(format!("kaibo-{n}"))
    }

    fn notify(&self, update: SessionUpdate) {
        let notif = SessionNotification::new(self.session_id.clone(), update);
        let _ = self.cx.send_notification(notif);
    }

    /// Meta narration about the turn as a whole (not a specific tool call) — an
    /// `AgentThoughtChunk` carrying kaibo's own `PhaseEvent::message()` one-liner,
    /// the same text the MCP progress notifications and the CLI's `TerminalSink`
    /// show.
    fn thought(&self, text: String) {
        self.notify(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )));
    }
}

impl std::fmt::Debug for AcpProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpProgressSink")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl ProgressSink for AcpProgressSink {
    fn emit(&self, event: PhaseEvent) {
        let msg = event.message();
        match event {
            PhaseEvent::PhaseStarted { .. }
            | PhaseEvent::PhaseFinished { .. }
            | PhaseEvent::TurnCapReached => self.thought(msg),
            // Announced before the script runs (see `explorer.rs`) and kaibo has no
            // matching "finished" beat for one today — reported honestly as
            // in-progress rather than faking a completion kaibo never observed.
            PhaseEvent::KaishRun { script } => {
                let id = self.mint_tool_id();
                self.notify(SessionUpdate::ToolCall(
                    ToolCall::new(id, msg)
                        .kind(ToolKind::Execute)
                        .status(ToolCallStatus::InProgress)
                        .raw_input(serde_json::json!({ "script": script })),
                ));
            }
            PhaseEvent::SweepStarted { .. } => {
                let id = self.mint_tool_id();
                *self
                    .current_sweep
                    .lock()
                    .expect("acp progress sink mutex poisoned") = Some(id.clone());
                self.notify(SessionUpdate::ToolCall(
                    ToolCall::new(id, msg)
                        .kind(ToolKind::Search)
                        .status(ToolCallStatus::InProgress),
                ));
            }
            PhaseEvent::SweepFinished => {
                let current = self
                    .current_sweep
                    .lock()
                    .expect("acp progress sink mutex poisoned")
                    .take();
                match current {
                    Some(id) => self.notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id,
                        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                    ))),
                    // No open sweep to close (shouldn't happen — the engine pairs
                    // these 1:1 — but narrate rather than silently drop the beat).
                    None => self.thought(msg),
                }
            }
            PhaseEvent::Attached { .. } => {
                let current = self
                    .current_sweep
                    .lock()
                    .expect("acp progress sink mutex poisoned")
                    .clone();
                match current {
                    Some(id) => self.notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id,
                        ToolCallUpdateFields::new().content(vec![ToolCallContent::from(
                            ContentBlock::Text(TextContent::new(msg)),
                        )]),
                    ))),
                    None => self.thought(msg),
                }
            }
        }
    }
}

/// Build the ACP agent connection: `initialize`, `session/new`, `session/prompt`,
/// `session/set_mode`, and `session/cancel`, wired to `state`. Returns an opaque
/// `ConnectTo<Client>` — the caller feeds it a transport with `.connect_to(transport)`
/// (`Stdio::new()` for the real CLI, an in-memory duplex in tests).
pub fn agent(state: AcpAgentState) -> impl ConnectTo<Client> {
    Agent
        .builder()
        .name("kaibo")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                // No auth: kaibo has its own read-only sandbox (`src/sandbox.rs`), so
                // there is nothing for an ACP auth method to gate. We only speak v1 —
                // negotiating anything else here would be a lie the client can't act on.
                let _ = req.protocol_version;
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new())
                        .auth_methods(Vec::new())
                        .agent_info(Implementation::new("kaibo", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: NewSessionRequest, responder, _cx| {
                    let session_id = state.mint_session_id();
                    state.insert_session(session_id.clone(), req.cwd.clone());
                    let modes =
                        SessionModeState::new(state.inner.default_cast.clone(), state.session_modes());
                    tracing::debug!(
                        session_id = %session_id,
                        cwd = %req.cwd.display(),
                        "acp: session/new"
                    );
                    responder.respond(NewSessionResponse::new(session_id).modes(modes))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: PromptRequest, responder, cx| {
                    let Some(record) = state.session(&req.session_id) else {
                        return responder.respond_with_error(
                            Error::invalid_params()
                                .data(format!("unknown session {}", req.session_id)),
                        );
                    };

                    let question = match extract_prompt_text(&req.prompt) {
                        Ok(q) => q,
                        Err(msg) => {
                            return responder
                                .respond_with_error(Error::invalid_params().data(msg))
                        }
                    };

                    let cast = match state.resolve_cast(&record.mode_id) {
                        Ok(c) => c,
                        Err(msg) => {
                            return responder
                                .respond_with_error(Error::invalid_params().data(msg))
                        }
                    };
                    let explorer = match state.arm(&cast, ModelRole::Explorer) {
                        Ok(a) => a,
                        Err(msg) => {
                            return responder
                                .respond_with_error(Error::internal_error().data(msg))
                        }
                    };
                    let synth = match state.arm(&cast, ModelRole::Synth) {
                        Ok(a) => a,
                        Err(msg) => {
                            return responder
                                .respond_with_error(Error::internal_error().data(msg))
                        }
                    };

                    let root = record.cwd.clone();
                    let house_rules = match state.inner.resolver.house_rules(&root) {
                        Ok(h) => h,
                        Err(e) => {
                            return responder.respond_with_error(
                                Error::internal_error().data(e.message.to_string()),
                            )
                        }
                    };
                    let orientation = match state.inner.resolver.orientation(&root).await {
                        Ok(o) => o,
                        Err(e) => {
                            return responder.respond_with_error(
                                Error::internal_error().data(e.message.to_string()),
                            )
                        }
                    };
                    let prompts = state.inner.resolver.resolved_prompts(&cast);
                    let defaults = &state.inner.resolver.config.defaults;

                    // Progress rides the whole turn: a clone feeds `ConsultConfig` (the
                    // spawned consult loop emits through it), and this one bookends the
                    // turn exactly as the MCP `consult` tool does — started here,
                    // finished once the spawned task actually lands an answer.
                    let progress: Arc<dyn ProgressSink> =
                        Arc::new(AcpProgressSink::new(cx.clone(), req.session_id.clone()));
                    progress.emit(PhaseEvent::PhaseStarted { phase: "consult" });

                    let cfg = ConsultConfig {
                        explore: ExploreConfig {
                            phase: PhaseContext {
                                progress: progress.clone(),
                                house_rules,
                                prompts,
                                orientation,
                                call_deadline: defaults.call_deadline,
                            },
                            explorer_max_turns: defaults.explorer_max_turns,
                            sandbox: state.inner.resolver.config.sandbox.clone(),
                            max_attachments: defaults.max_attachments,
                        },
                        synth_max_turns: defaults.synth_max_turns,
                        attachments: Vec::new(),
                    };

                    let explorer_for_task = explorer.clone();
                    let synth_for_task = synth.clone();
                    let sessions_for_task = state.inner.consult_sessions.clone();
                    let session_key = req.session_id.to_string();

                    // Spawn the actual model work on its own task so this handler can
                    // return immediately: the connection's event loop can't read the
                    // next message (a `session/cancel`, most importantly) while a
                    // handler's own future is still running. `tokio::spawn` (not
                    // `cx.spawn`) because we need its `AbortHandle` — `cx.spawn` runs the
                    // task but doesn't hand one back.
                    let consult_task = tokio::spawn(async move {
                        consult(
                            &question,
                            None,
                            root,
                            &explorer_for_task,
                            &synth_for_task,
                            &cfg,
                            Some((&sessions_for_task, &session_key)),
                        )
                        .await
                    });
                    state.set_running(&req.session_id, consult_task.abort_handle());

                    let explorer_model = explorer.model.clone();
                    let synth_model = synth.model.clone();
                    let cast_name = cast.name.clone();
                    let session_id_for_task = req.session_id.clone();
                    let state_for_task = state.clone();
                    let cx_for_task = cx.clone();

                    cx.spawn(async move {
                        let outcome = consult_task.await;
                        state_for_task.clear_running(&session_id_for_task);
                        let response = match outcome {
                            Ok(Ok(out)) => {
                                progress.emit(PhaseEvent::PhaseFinished { phase: "consult" });
                                let answer = with_provenance(
                                    append_warnings(out.answer, &out.warnings),
                                    &cast_name,
                                    &[
                                        ("explorer", explorer_model.as_str()),
                                        ("synth", synth_model.as_str()),
                                    ],
                                    &out.usage,
                                );
                                let update = SessionNotification::new(
                                    session_id_for_task.clone(),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(answer)),
                                    )),
                                );
                                cx_for_task.send_notification(update)?;
                                Ok(PromptResponse::new(StopReason::EndTurn))
                            }
                            // A provider/model-loop failure — the turn ran and failed,
                            // not a bad request. Named and classified the same way the
                            // MCP `consult` tool renders a consultation failure.
                            Ok(Err(e)) => Err(Error::internal_error().data(
                                consultation_failure_text("session/prompt", &cast_name, e),
                            )),
                            // `session/cancel` aborted the spawned task: the spec's own
                            // stop reason for exactly this.
                            Err(join_err) if join_err.is_cancelled() => {
                                Ok(PromptResponse::new(StopReason::Cancelled))
                            }
                            // The task panicked — a kaibo-side bug, not a provider
                            // failure or a cancellation.
                            Err(join_err) => Err(Error::internal_error().data(format!(
                                "the consult task ended unexpectedly: {join_err}"
                            ))),
                        };
                        responder.respond_with_result(response)
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: SetSessionModeRequest, responder, _cx| {
                    let mode_id = req.mode_id.to_string();
                    if !state.known_mode(&mode_id) {
                        return responder.respond_with_error(
                            Error::invalid_params()
                                .data(format!("unknown session mode {mode_id}")),
                        );
                    }
                    if !state.set_mode(&req.session_id, mode_id.clone()) {
                        return responder.respond_with_error(
                            Error::invalid_params()
                                .data(format!("unknown session {}", req.session_id)),
                        );
                    }
                    tracing::debug!(session_id = %req.session_id, mode = %mode_id, "acp: session/set_mode");
                    responder.respond(SetSessionModeResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                async move |notif: CancelNotification, _cx| {
                    // Best-effort and cooperative, per the spec: aborting the in-flight
                    // task (if any) is all this needs to do — the prompt handler's own
                    // spawned tail observes the abort and answers `StopReason::Cancelled`.
                    // A notification carries no response, so an unknown session id is
                    // logged, not erred; a known-but-idle session is a silent no-op.
                    if !state.cancel_running(&notif.session_id) {
                        tracing::debug!(
                            session_id = %notif.session_id,
                            "acp: session/cancel for unknown session"
                        );
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_client_protocol::schema::v1::{
        NewSessionRequest as ClientNewSessionRequest, PromptRequest as ClientPromptRequest,
        SetSessionModeRequest as ClientSetSessionModeRequest,
    };
    use agent_client_protocol::{ByteStreams, Client as ClientRole};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use crate::consult::ModelCaps;
    use crate::test_support::{text_response, transcript_text, ScriptedClient};

    /// A resolved [`Config`] carrying only the fields this module reads (`casts`,
    /// `default_cast`) via `Config::builtin()` — the built-in casts (`anthropic`,
    /// `deepseek`, `gemini`, `openrouter`, `openai-local`) are what a fresh install
    /// without a config.toml resolves to, so testing against them here tests the shape
    /// a real first run advertises.
    fn test_config() -> Config {
        Config::builtin()
    }

    /// A scripted arm over `client`, addressing model `model` — same shape
    /// `consult`'s own offline tests build (`src/consult/engine.rs`), vision off
    /// (nothing here attaches an image).
    fn scripted_arm(client: &ScriptedClient, model: &str) -> Arm {
        Arm::new(
            client.clone(),
            model,
            16384,
            None,
            ModelCaps {
                vision: false,
                tool_result_images: true,
            },
        )
    }

    /// One in-memory duplex, split and wrapped as two independent [`ByteStreams`]
    /// transports — real JSON-RPC bytes over `tokio::io::duplex`, no network, no
    /// stdio. `tokio_util`'s `compat` layer bridges tokio's `AsyncRead`/`AsyncWrite` to
    /// the `futures` traits `ByteStreams` requires.
    fn new_transport_pair() -> (
        impl ConnectTo<Agent> + 'static,
        impl ConnectTo<Client> + 'static,
    ) {
        let (agent_io, client_io) = tokio::io::duplex(1 << 20);
        let (agent_read, agent_write) = tokio::io::split(agent_io);
        let (client_read, client_write) = tokio::io::split(client_io);
        (
            ByteStreams::new(agent_write.compat_write(), agent_read.compat()),
            ByteStreams::new(client_write.compat_write(), client_read.compat()),
        )
    }

    /// A project root with one real file, so orientation/house-rules assembly (which
    /// spawns a real read-only kaish worker) has something to mount — a turn's cwd
    /// must be a real directory even though the model side is fully scripted.
    fn project_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "kaibo acp test fixture\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn initialize_negotiates_v1_with_no_auth_methods() {
        let state = AcpAgentState::new(Arc::new(test_config())).expect("resolver builds");
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let response = ClientRole
            .builder()
            .name("test-client")
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        assert!(response.auth_methods.is_empty());
    }

    #[tokio::test]
    async fn session_new_returns_an_id_and_advertises_cast_modes() {
        let state = AcpAgentState::new(Arc::new(test_config())).expect("resolver builds");
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let response = ClientRole
            .builder()
            .name("test-client")
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(ClientNewSessionRequest::new("/tmp/kaibo-acp-test"))
                    .block_task()
                    .await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert!(!response.session_id.to_string().is_empty());
        let modes = response.modes.expect("session modes should be advertised");
        let mut mode_ids: Vec<String> = modes
            .available_modes
            .iter()
            .map(|m| m.id.to_string())
            .collect();
        mode_ids.sort();
        let mut expected: Vec<String> = test_config().casts.keys().cloned().collect();
        expected.sort();
        assert_eq!(mode_ids, expected);
        assert_eq!(
            modes.current_mode_id.to_string(),
            test_config().default_cast
        );
    }

    #[test]
    fn known_mode_matches_a_configured_cast_name() {
        let state = AcpAgentState::new(Arc::new(test_config())).expect("resolver builds");
        assert!(state.known_mode(&test_config().default_cast));
        assert!(!state.known_mode("not-a-real-cast"));
    }

    #[test]
    fn set_mode_on_an_unknown_session_reports_failure() {
        let state = AcpAgentState::new(Arc::new(test_config())).expect("resolver builds");
        assert!(!state.set_mode(&SessionId::new("nope"), "anthropic".to_string()));
    }

    /// The full path, scripted: `session/prompt` drives the REAL consult loop (offline,
    /// via `ScriptedClient`) and the caller sees at least one progress-derived
    /// `session/update` before the final answer, which carries the provenance footer.
    #[tokio::test]
    async fn prompt_runs_the_real_consult_loop_and_streams_progress_before_the_answer() {
        let client = ScriptedClient::builder()
            .on_model("scripted-synth", |req| {
                let shown = transcript_text(req);
                Ok(text_response(format!("ANSWER seeing[{shown}]")))
            })
            .build();
        let default_cast = test_config().default_cast;
        let mut arms = HashMap::new();
        arms.insert(
            default_cast.clone(),
            (
                scripted_arm(&client, "scripted-explorer"),
                scripted_arm(&client, "scripted-synth"),
            ),
        );
        let state = AcpAgentState::new_scripted(&test_config(), arms);
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = updates.clone();
        let root = project_dir();
        let root_path = root.path().display().to_string();

        let prompt_response = ClientRole
            .builder()
            .name("test-client")
            .on_receive_notification(
                {
                    let updates = updates.clone();
                    async move |notif: SessionNotification, _cx| {
                        updates
                            .lock()
                            .expect("updates mutex poisoned")
                            .push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                cx.send_request(ClientPromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("what does this do?"))],
                ))
                .block_task()
                .await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert_eq!(prompt_response.stop_reason, StopReason::EndTurn);
        let updates = recorded.lock().expect("updates mutex poisoned").clone();
        let answer_pos = updates
            .iter()
            .position(|u| matches!(u, SessionUpdate::AgentMessageChunk(_)))
            .expect("a final AgentMessageChunk answer must be sent");
        assert!(
            answer_pos > 0,
            "at least one progress-derived update must precede the answer, got: {updates:?}"
        );
        match &updates[answer_pos] {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => {
                    assert!(
                        text.text.contains("ANSWER"),
                        "answer text present: {}",
                        text.text
                    );
                    assert!(
                        text.text.contains(&format!("cast `{default_cast}`")),
                        "provenance names the cast: {}",
                        text.text
                    );
                    assert!(
                        text.text.contains("scripted-synth"),
                        "provenance names the answering model: {}",
                        text.text
                    );
                }
                other => panic!("expected a text content block, got {other:?}"),
            },
            other => panic!("expected an AgentMessageChunk update, got {other:?}"),
        }
    }

    /// A second prompt in the same ACP session replays the first turn's Q&A: the
    /// scripted synth reads its own prior answer back in the transcript, proving
    /// `session/prompt` threads the real `consult_session_turn` replay/record
    /// machinery (keyed by the ACP session id), not a stateless call each time.
    #[tokio::test]
    async fn a_second_prompt_in_the_same_session_replays_prior_context() {
        let client = ScriptedClient::builder()
            .on_model("scripted-synth", |req| {
                let shown = transcript_text(req);
                if shown.contains("first turn marker") {
                    Ok(text_response("SECOND[saw the first turn]"))
                } else {
                    Ok(text_response("FIRST[first turn marker]"))
                }
            })
            .build();
        let default_cast = test_config().default_cast;
        let mut arms = HashMap::new();
        arms.insert(
            default_cast,
            (
                scripted_arm(&client, "scripted-explorer"),
                scripted_arm(&client, "scripted-synth"),
            ),
        );
        let state = AcpAgentState::new_scripted(&test_config(), arms);
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let root = project_dir();
        let root_path = root.path().display().to_string();

        let (first, second) = ClientRole
            .builder()
            .name("test-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                let first = cx
                    .send_request(ClientPromptRequest::new(
                        session.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("first question"))],
                    ))
                    .block_task()
                    .await?;
                let second = cx
                    .send_request(ClientPromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new("second question"))],
                    ))
                    .block_task()
                    .await?;
                Ok((first, second))
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert_eq!(first.stop_reason, StopReason::EndTurn);
        assert_eq!(second.stop_reason, StopReason::EndTurn);
    }

    /// `session/set_mode` switches which cast the NEXT prompt resolves to: two casts
    /// in the built-in registry (`anthropic`, `deepseek`), each wired to a distinct
    /// scripted model id, so the answering model's name in the provenance footer
    /// proves the switch actually took.
    #[tokio::test]
    async fn set_mode_then_prompt_uses_the_newly_selected_casts_model() {
        let client = ScriptedClient::builder()
            .on_model("scripted-synth-a", |_req| Ok(text_response("from A")))
            .on_model("scripted-synth-b", |_req| Ok(text_response("from B")))
            .build();
        let mut arms = HashMap::new();
        arms.insert(
            "anthropic".to_string(),
            (
                scripted_arm(&client, "scripted-explorer-a"),
                scripted_arm(&client, "scripted-synth-a"),
            ),
        );
        arms.insert(
            "deepseek".to_string(),
            (
                scripted_arm(&client, "scripted-explorer-b"),
                scripted_arm(&client, "scripted-synth-b"),
            ),
        );
        let state = AcpAgentState::new_scripted(&test_config(), arms);
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = updates.clone();
        let root = project_dir();
        let root_path = root.path().display().to_string();

        ClientRole
            .builder()
            .name("test-client")
            .on_receive_notification(
                {
                    let updates = updates.clone();
                    async move |notif: SessionNotification, _cx| {
                        updates
                            .lock()
                            .expect("updates mutex poisoned")
                            .push(notif.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                // Switch onto "deepseek" (mode id == cast name) before the turn.
                cx.send_request(ClientSetSessionModeRequest::new(
                    session.session_id.clone(),
                    "deepseek",
                ))
                .block_task()
                .await?;
                cx.send_request(ClientPromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("which model answers?"))],
                ))
                .block_task()
                .await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        let updates = recorded.lock().expect("updates mutex poisoned");
        let answer = updates
            .iter()
            .find_map(|u| match u {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("an answer chunk must have been sent");
        assert!(
            answer.contains("from B") && answer.contains("scripted-synth-b"),
            "switching to `deepseek` must route the turn to its scripted model, got: {answer}"
        );
        assert!(
            !answer.contains("from A"),
            "the `anthropic` cast's model must not have answered: {answer}"
        );
    }

    /// `session/cancel` mid-turn aborts the spawned consult task; the prompt's own
    /// response lands with `StopReason::Cancelled`, per spec. `hang_model` makes the
    /// synth's completion call park forever once invoked (see `test_support.rs`) —
    /// the deterministic "still running" shape, the same tool `jobs.rs`'s own cancel
    /// test uses a gated channel for. The request goes out (so `consult_task` is
    /// genuinely in flight, not raced against a fast mock answer), then never
    /// resolves on its own: only the abort ends it.
    #[tokio::test]
    async fn cancel_mid_turn_yields_the_cancelled_stop_reason() {
        let client = ScriptedClient::builder()
            .hang_model("scripted-synth")
            .build();

        let default_cast = test_config().default_cast;
        let mut arms = HashMap::new();
        arms.insert(
            default_cast,
            (
                scripted_arm(&client, "scripted-explorer"),
                scripted_arm(&client, "scripted-synth"),
            ),
        );
        let state = AcpAgentState::new_scripted(&test_config(), arms);
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let root = project_dir();
        let root_path = root.path().display().to_string();

        let response = ClientRole
            .builder()
            .name("test-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                let prompt = cx.send_request(ClientPromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("cancel me"))],
                ));
                cx.send_notification(CancelNotification::new(session.session_id))?;
                prompt.block_task().await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert_eq!(
            response.stop_reason,
            StopReason::Cancelled,
            "a session/cancel mid-turn must yield the spec's Cancelled stop reason"
        );
    }

    /// `session/cancel` with nothing running is a clean no-op: no panic, and a later
    /// prompt on the same session still runs normally.
    #[tokio::test]
    async fn cancel_with_no_in_flight_turn_is_a_clean_no_op() {
        let client = ScriptedClient::builder()
            .on_model("scripted-synth", |_req| Ok(text_response("still works")))
            .build();
        let default_cast = test_config().default_cast;
        let mut arms = HashMap::new();
        arms.insert(
            default_cast,
            (
                scripted_arm(&client, "scripted-explorer"),
                scripted_arm(&client, "scripted-synth"),
            ),
        );
        let state = AcpAgentState::new_scripted(&test_config(), arms);
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        let root = project_dir();
        let root_path = root.path().display().to_string();

        let response = ClientRole
            .builder()
            .name("test-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                // Nothing running yet — must be a silent no-op.
                cx.send_notification(CancelNotification::new(session.session_id.clone()))?;
                cx.send_request(ClientPromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("normal question"))],
                ))
                .block_task()
                .await
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        assert_eq!(
            response.stop_reason,
            StopReason::EndTurn,
            "an idle-session cancel must not disturb a subsequent normal turn"
        );
    }

    /// A non-text content block is refused up front with a clear error, never
    /// silently dropped or forwarded to the model.
    #[tokio::test]
    async fn a_non_text_content_block_is_refused() {
        let state = AcpAgentState::new(Arc::new(test_config())).expect("resolver builds");
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));
        let root = project_dir();
        let root_path = root.path().display().to_string();

        let result = ClientRole
            .builder()
            .name("test-client")
            .connect_with(client_transport, async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(ClientNewSessionRequest::new(root_path))
                    .block_task()
                    .await?;
                Ok(cx
                    .send_request(ClientPromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Image(
                            agent_client_protocol::schema::v1::ImageContent::new(
                                "aGk=",
                                "image/png",
                            ),
                        )],
                    ))
                    .block_task()
                    .await)
            })
            .await
            .expect("client-side connection failed");

        agent_task
            .await
            .expect("agent task panicked")
            .expect("agent-side connection failed");

        let err = result.expect_err("an image content block must be refused");
        assert!(
            err.message.contains("text content blocks")
                || err
                    .data
                    .as_ref()
                    .map(|d| d.to_string().contains("text content blocks"))
                    .unwrap_or(false),
            "the refusal names the constraint: {err:?}"
        );
    }
}
