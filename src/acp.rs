//! `kaibo acp` — the Agent Client Protocol front door.
//!
//! kaibo's second protocol front door, alongside MCP (`src/server/`) and the CLI
//! (`src/cli.rs`): an ACP v1 agent, so an ACP client (Zed, Toad, JetBrains) drives
//! kaibo the same way an MCP client does, over stdio. This chunk is scaffold and
//! handshake only — `initialize`, `session/new`, `session/prompt`, `session/cancel`,
//! and `session/set_mode` all answer, but `session/prompt` returns a canned reply
//! instead of running the real consult loop. Wiring `run_kaish`/the model team into
//! the prompt turn is chunk 2.
//!
//! ACP's session concept doesn't map onto kaibo's `consult` session (a lean
//! question/answer history replayed as context, `src/session.rs`) or the durable
//! store (`src/store.rs`): an ACP session is a whole client conversation — a cwd,
//! an MCP server list, a mode — that a prompt turn runs inside. This chunk keeps
//! that mapping in a small in-memory table, keyed by a `session-N` id (the `job-N`
//! style already used in `src/jobs.rs`); chunk 2 decides how much of it, if any,
//! rides on the existing store.
//!
//! # Executor
//!
//! `agent-client-protocol` 1.3.0's connection is executor-agnostic and `Send`
//! throughout: `ConnectTo::connect_to` returns `impl Future<..> + Send`, and every
//! handler closure registered below must itself be `Send`. This is unlike the `!Send`
//! kaish kernel (`src/sandbox.rs`'s `KaishWorker`, run on its own thread because rig
//! tools require `Send` futures and the kernel's execution future is not one) — no
//! `LocalSet` or dedicated thread is needed here, and none is added. When chunk 2
//! wires the real consult loop into `session/prompt`, it drives `KaishWorker` through
//! its already-`Send` channel handle, the same way `run_kaish` does today; the `!Send`
//! kernel stays on its own worker thread regardless of which front door called it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionMode, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectTo, Error};

use crate::config::Config;

/// The canned `session/prompt` reply for this chunk. Plain declarative text — this
/// string rides over the wire to a real ACP client (Zed, Toad), not just a log line.
const SCAFFOLD_REPLY: &str =
    "kaibo ACP scaffold. The real consult loop is not wired yet. It lands in chunk 2.";

/// One ACP session this scaffold has minted: the client's working directory (read by
/// chunk 2's consult wiring, unused here beyond the debug log below) and which cast
/// the session is currently pinned to.
#[derive(Debug, Clone)]
struct SessionRecord {
    cwd: PathBuf,
    mode_id: String,
}

/// Shared state behind every ACP connection this process serves: the cast roster
/// (each name becomes one advertised session mode) and the in-memory session table.
/// `Clone` is cheap — an `Arc` around the mutable inner state — so each registered
/// handler closure gets its own handle to the same table.
#[derive(Clone)]
pub struct AcpAgentState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Configured cast names, in `Config::casts`'s `BTreeMap` order — deterministic,
    /// so the advertised mode list doesn't reorder between calls.
    cast_names: Vec<String>,
    default_cast: String,
    sessions: Mutex<HashMap<SessionId, SessionRecord>>,
    next_session: AtomicU64,
}

impl AcpAgentState {
    /// Build the shared state from a resolved [`Config`]: one session mode per
    /// configured cast, starting on the config's default cast.
    pub fn new(config: &Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                cast_names: config.casts.keys().cloned().collect(),
                default_cast: config.default_cast.clone(),
                sessions: Mutex::new(HashMap::new()),
                next_session: AtomicU64::new(1),
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
                    tracing::debug!(
                        session_id = %req.session_id,
                        cwd = %record.cwd.display(),
                        mode = %record.mode_id,
                        "acp: session/prompt (scaffold canned reply, chunk 2 wires the real loop)"
                    );
                    let update = SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new(SCAFFOLD_REPLY),
                        ))),
                    );
                    cx.send_notification(update)?;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
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
                    // Chunk 1 has no in-flight tool loop: `session/prompt` above already
                    // finished by the time a client could send this. Nothing to stop yet
                    // — chunk 2's real loop is the one that needs a cancellation flag to
                    // check. A notification carries no response, so an unknown session id
                    // is logged, not erred.
                    if state.session(&notif.session_id).is_none() {
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
    };
    use agent_client_protocol::{ByteStreams, Client as ClientRole};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// A resolved [`Config`] carrying only the fields this module reads (`casts`,
    /// `default_cast`) via `Config::builtin()` — the built-in casts (`anthropic`,
    /// `deepseek`, `gemini`, `openrouter`, `openai-local`) are what a fresh install
    /// without a config.toml resolves to, so testing against them here tests the shape
    /// a real first run advertises.
    fn test_config() -> Config {
        Config::builtin()
    }

    /// One in-memory duplex, split and wrapped as two independent [`ByteStreams`]
    /// transports — real JSON-RPC bytes over `tokio::io::duplex`, no network, no
    /// stdio. `tokio_util`'s `compat` layer bridges tokio's `AsyncRead`/`AsyncWrite` to
    /// the `futures` traits `ByteStreams` requires.
    fn new_transport_pair() -> (
        impl ConnectTo<Agent> + 'static,
        impl ConnectTo<Client> + 'static,
    ) {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        let (agent_read, agent_write) = tokio::io::split(agent_io);
        let (client_read, client_write) = tokio::io::split(client_io);
        (
            ByteStreams::new(agent_write.compat_write(), agent_read.compat()),
            ByteStreams::new(client_write.compat_write(), client_read.compat()),
        )
    }

    #[tokio::test]
    async fn initialize_negotiates_v1_with_no_auth_methods() {
        let state = AcpAgentState::new(&test_config());
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
        let state = AcpAgentState::new(&test_config());
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

    #[tokio::test]
    async fn prompt_yields_the_canned_update_then_a_completed_turn() {
        let state = AcpAgentState::new(&test_config());
        let (agent_transport, client_transport) = new_transport_pair();
        let agent_task = tokio::spawn(agent(state).connect_to(agent_transport));

        // The client-side notification handler must be registered on the builder
        // BEFORE `connect_with` — `ConnectionTo` (the `cx` handed to `connect_with`'s
        // closure) has no way to add one mid-connection, only `Builder` does.
        let updates: std::sync::Arc<std::sync::Mutex<Vec<SessionUpdate>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = updates.clone();

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
                    .send_request(ClientNewSessionRequest::new("/tmp/kaibo-acp-test"))
                    .block_task()
                    .await?;
                cx.send_request(ClientPromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("hello"))],
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
        let updates = recorded.lock().expect("updates mutex poisoned");
        assert_eq!(updates.len(), 1, "expected exactly one session/update");
        match &updates[0] {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => assert_eq!(text.text, SCAFFOLD_REPLY),
                other => panic!("expected a text content block, got {other:?}"),
            },
            other => panic!("expected an AgentMessageChunk update, got {other:?}"),
        }
    }

    #[test]
    fn known_mode_matches_a_configured_cast_name() {
        let state = AcpAgentState::new(&test_config());
        assert!(state.known_mode(&test_config().default_cast));
        assert!(!state.known_mode("not-a-real-cast"));
    }

    #[test]
    fn set_mode_on_an_unknown_session_reports_failure() {
        let state = AcpAgentState::new(&test_config());
        assert!(!state.set_mode(&SessionId::new("nope"), "anthropic".to_string()));
    }
}
