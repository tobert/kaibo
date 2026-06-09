//! The MCP server surface: one `consult` tool over the two-phase pipeline.
//!
//! stdio only — like kaish-mcp, kaibo must never bind a socket: it can read a
//! user's filesystem, so the transport pipe is the security boundary.

use std::path::PathBuf;

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use kaish_kernel::tools::ToolSchema;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, Implementation, ListResourceTemplatesResult,
    ListResourcesResult, LoggingLevel, Meta, PaginatedRequestParams, ProgressNotificationParam,
    ProgressToken, ProtocolVersion, RawResource, RawResourceTemplate, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, SetLevelRequestParams,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{Peer, RequestContext};
use rmcp::ErrorData as McpError;
use rmcp::{tool, tool_handler, tool_router, RoleServer};
use serde::Deserialize;
use serde_json::json;

use crate::config::{Config, Profile};
use crate::consult::{consult, explore, synthesize, ConsultConfig};
use crate::explorer::format_output;
use crate::kaish_syntax::{kaibo_sandbox_doc, kaibo_instructions, render_builtin_help, render_topic, topics};
use crate::mcp_log;
use crate::progress::{NullSink, PhaseEvent, ProgressSink};
use crate::sandbox::{builtin_schemas, KaishWorker};
use crate::session::SessionStore;

/// kaibo's resource URI namespace. Everything kaish-related hangs off `kaibo://kaish/`.
const KAISH_RES_PREFIX: &str = "kaibo://kaish/";
/// kaibo's own read-only boundary doc (replaces the old `kaibo://kaish-syntax`).
const SANDBOX_URI: &str = "kaibo://kaish/sandbox";
/// Per-builtin help, addressed by name: `kaibo://kaish/builtin/grep`.
const BUILTIN_PREFIX: &str = "kaibo://kaish/builtin/";
/// The URI template advertised for the per-builtin resources.
const BUILTIN_URI_TEMPLATE: &str = "kaibo://kaish/builtin/{name}";

/// Which tools to advertise. All on by default; each `--no-<tool>` flips one off.
///
/// Composes to any posture: `{explore:false, synthesize:false}` ≈ the original
/// consult-only surface; only `run_kaish` on ≈ "no code leaves the box, kaibo as a
/// pure read-only shell". A server with *all* off is a misconfiguration — refused
/// at startup (see `main`), not represented as a valid state here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolGating {
    pub consult: bool,
    pub explore: bool,
    pub synthesize: bool,
    pub run_kaish: bool,
}

impl Default for ToolGating {
    fn default() -> Self {
        Self { consult: true, explore: true, synthesize: true, run_kaish: true }
    }
}

impl ToolGating {
    /// True iff every tool is disabled — the zero-tool server we refuse to start.
    pub fn all_disabled(&self) -> bool {
        !self.consult && !self.explore && !self.synthesize && !self.run_kaish
    }
}

/// Arguments to the `consult` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsultInput {
    /// The question to investigate about the project.
    pub question: String,

    /// Absolute path to the project to explore. Optional only if the server was
    /// launched with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider/profile: a built-in kind ("anthropic" (default), "deepseek",
    /// "gemini", "openai") or a profile name from config.toml.
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the explorer (investigation) model id.
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Override the synthesizer (final-answer) model id.
    #[serde(default)]
    pub synth_model: Option<String>,

    /// Opaque session id to make this a multi-turn consult. When set, kaibo replays
    /// this session's prior `(question, answer)` pairs as context and records this
    /// turn into it; the exploration still runs fresh. Omit it for a stateless,
    /// one-shot consult. Sessions are evicted by capacity, not time.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Max tool-loop turns for each delegated `explore′` sweep (default 50).
    #[serde(default)]
    pub explorer_max_turns: Option<usize>,

    /// Max tool-loop turns for the consult driver loop itself (default 100 — it now
    /// drives the whole investigation, delegating sweeps and reading spans).
    #[serde(default)]
    pub synth_max_turns: Option<usize>,

    /// Surface the explorer's aggregated report (the `explore′` sweeps the consult
    /// delegated) as `structured_content` alongside the answer. Off by default: the
    /// report can be large and most clients feed structured content to the model, so
    /// a normal consult stays lean — opt in for "show your work" / debugging the
    /// hand-off. When on, the report rides separately and is surfaced even if empty
    /// (an empty report is itself the signal that the consult delegated no sweep).
    #[serde(default)]
    pub include_report: bool,
}

/// Arguments to the `explore` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreInput {
    /// The question to investigate about the project.
    pub question: String,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider: "anthropic" (default), "deepseek", "gemini", or "openai".
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the explorer model id.
    #[serde(default)]
    pub model: Option<String>,

    /// Max tool-loop turns for the explorer (default 50 — it's cheap, let it rip).
    #[serde(default)]
    pub max_turns: Option<usize>,
}

/// Arguments to the `synthesize` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SynthesizeInput {
    /// The question to answer.
    pub question: String,

    /// Optional context to ground the answer in — typically an `explore` report or
    /// pasted source. When absent, the model investigates via `run_kaish`.
    #[serde(default)]
    pub context: Option<String>,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`.
    #[serde(default)]
    pub path: Option<String>,

    /// Provider: "anthropic" (default), "deepseek", "gemini", or "openai".
    #[serde(default)]
    pub provider: Option<String>,

    /// Override the synthesizer (capable) model id.
    #[serde(default)]
    pub model: Option<String>,
}

/// Arguments to the `run_kaish` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunKaishInput {
    /// The kaish (sh-like) script to run against the read-only project.
    pub script: String,

    /// Absolute path to the project. Optional only if the server was launched
    /// with a default `--root`. Each call starts at this root — there is no
    /// persistent cwd across calls.
    #[serde(default)]
    pub path: Option<String>,
}

/// kaibo's MCP handler. Cheap to clone (rmcp clones it per request).
#[derive(Clone)]
pub struct KaiboHandler {
    /// The resolved configuration: profile registry, defaults, default root and
    /// provider. `Arc` because rmcp clones the handler per request and it's
    /// immutable after startup.
    config: Arc<Config>,
    tool_router: ToolRouter<Self>,
    /// The kernel's builtin schemas, snapshotted once at startup. Drives the
    /// `kaibo://kaish/*` help resources and the composed onboarding instructions.
    /// `Arc` because rmcp clones the handler per request and these never change.
    tool_schemas: Arc<Vec<ToolSchema>>,
    /// Multi-turn `consult` sessions. Internally an `Arc<Mutex<_>>`, so the
    /// per-request handler clones all share one cache (see [`SessionStore`]).
    sessions: SessionStore,
    /// The client's MCP log floor (a [`mcp_log::rank`]), written by `logging/setLevel`
    /// and read by the log-drain task. `Arc<AtomicU8>` so every per-request handler
    /// clone — and the drain task in `main` — share the one cell; a `setLevel` on any
    /// request takes effect immediately for the whole server.
    mcp_log_level: Arc<AtomicU8>,
}

#[tool_router]
impl KaiboHandler {
    /// Build the handler from a resolved [`Config`]. Snapshots the kernel's builtin
    /// schemas up front (a cheap in-memory kernel); a failure here is a broken build,
    /// surfaced at startup rather than papered over with an empty help surface.
    pub fn new(config: Config) -> Result<Self> {
        let gating = config.tools;
        // `#[tool_router]` gathers every #[tool] method at compile time; gating is a
        // runtime choice, so build the full router and drop the disabled routes by
        // name. (The methods stay compiled — no dead code — they're just not
        // advertised or callable.)
        let mut tool_router = Self::tool_router();
        // `remove_route` silently no-ops on an unknown name, so a renamed #[tool]
        // method would leave its --no-<tool> flag quietly inert. Assert the route
        // exists before dropping it — a stale name is a build-time bug we want loud.
        for (enabled, name) in [
            (gating.consult, "consult"),
            (gating.explore, "explore"),
            (gating.synthesize, "synthesize"),
            (gating.run_kaish, "run_kaish"),
        ] {
            if !enabled {
                assert!(
                    tool_router.has_route(name),
                    "gating: no tool route named {name:?} — did a #[tool] method get renamed?"
                );
                tool_router.remove_route(name);
            }
        }
        let sessions = SessionStore::new(config.defaults.session_capacity);
        Ok(Self {
            config: Arc::new(config),
            tool_router,
            tool_schemas: Arc::new(builtin_schemas()?),
            sessions,
            mcp_log_level: Arc::new(AtomicU8::new(mcp_log::rank(mcp_log::DEFAULT_LEVEL))),
        })
    }

    /// A handle to the shared MCP log floor, for the drain task in `main` to read.
    /// Cloned, not borrowed, because the drain outlives this `&self`.
    pub fn mcp_log_level(&self) -> Arc<AtomicU8> {
        self.mcp_log_level.clone()
    }

    /// Set the MCP log floor. The body of `set_level`, split out so the level logic is
    /// testable without fabricating a `RequestContext` (which needs a non-public peer).
    pub fn apply_log_level(&self, level: LoggingLevel) {
        self.mcp_log_level.store(mcp_log::rank(level), Ordering::Relaxed);
    }

    /// Tool names this handler advertises, after gating. For tests/diagnostics.
    pub fn advertised_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// Resolve a call's project root: the explicit `path`, else the server's
    /// `--root`. A call with neither is a parameter error, not a silent default.
    fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        match path {
            Some(p) => Ok(PathBuf::from(p)),
            None => self.config.root.clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no `path` provided and the server has no default --root",
                    None,
                )
            }),
        }
    }

    /// Resolve a call's provider profile: the explicit name (looked up in the
    /// registry, by name or alias), else the server's default profile. An unknown
    /// name is a parameter error naming the available profiles. Returns an owned
    /// clone so the caller can layer per-call model overrides onto it.
    fn resolve_profile(&self, provider: Option<String>) -> Result<Profile, McpError> {
        let name = provider.unwrap_or_else(|| self.config.default_provider.clone());
        self.config
            .resolve_profile(&name)
            .cloned()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    #[tool(
        description = "Investigate a question about a codebase and return a grounded, \
            cited answer. A capable model drives a read-only kaish shell \
            (cat/grep/rg/find/jq/pipelines): it reads precise spans directly and \
            delegates broad repo sweeps to a fast explorer sub-agent, then answers \
            from what they find with concrete `file:line` citations. Read-only: it \
            never modifies the project. For a multi-turn conversation, pass a stable \
            session_id: kaibo replays that session's prior question/answer pairs as \
            context (the exploration still runs fresh each turn). Args: question \
            (required), path (project dir; optional if the server has a default root), \
            provider (anthropic|deepseek|gemini|openai), session_id (optional; opaque \
            conversation key), include_report (optional; attach the explorer's \
            report as structured_content for debugging the hand-off), and optional \
            explorer_model / synth_model overrides."
    )]
    async fn consult(
        &self,
        Parameters(input): Parameters<ConsultInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        // Resolve the profile, then layer per-call model overrides onto the clone.
        let mut profile = self.resolve_profile(input.provider)?;
        if let Some(m) = input.explorer_model {
            profile.explorer_model = m;
        }
        if let Some(m) = input.synth_model {
            profile.synth_model = m;
        }
        // Progress rides the whole investigation: sweeps and direct reads emit beats
        // onto the wire when the client supplied a token, else a no-op sink.
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let cfg = ConsultConfig {
            explorer_max_turns: input.explorer_max_turns.unwrap_or(defaults.explorer_max_turns),
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            max_tokens: profile.max_tokens,
            sandbox: self.config.sandbox.clone(),
            progress: progress.clone(),
        };

        // Multi-turn: a session_id binds this turn to a thread (replay prior turns,
        // record this one); without one it's a stateless one-shot. The replay/record
        // glue lives in `consult_session_turn` (offline-tested) — the session mutex is
        // only ever touched there, never held across the consult await.
        let session = input.session_id.as_deref().map(|id| (&self.sessions, id));

        progress.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        let out = consult(&input.question, root, &profile, &cfg, session)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        progress.emit(PhaseEvent::PhaseFinished { phase: "consult" });

        Ok(consult_result(out.answer, out.report, input.include_report))
    }

    #[tool(
        description = "Investigate a question about a codebase and return a curated \
            report citing concrete `file:line` locations. A fast, cheap model reads \
            the project through a read-only kaish shell (cat/grep/rg/find/pipelines) \
            and reports what it found — relevant files, line numbers, key snippets. \
            This is the explorer seam on its own: it gathers evidence, it does not \
            write a polished final answer (use `consult` for that). Read-only: it \
            never modifies the project. Args: question (required), path (project dir; \
            optional if the server has a default root), provider \
            (anthropic|deepseek|gemini|openai), and optional model / max_turns \
            overrides."
    )]
    async fn explore(
        &self,
        Parameters(input): Parameters<ExploreInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let mut profile = self.resolve_profile(input.provider)?;
        if let Some(m) = input.model {
            profile.explorer_model = m;
        }
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let cfg = ConsultConfig {
            explorer_max_turns: input.max_turns.unwrap_or(defaults.explorer_max_turns),
            synth_max_turns: defaults.synth_max_turns,
            max_tokens: profile.max_tokens,
            sandbox: self.config.sandbox.clone(),
            progress: progress.clone(),
        };

        progress.emit(PhaseEvent::PhaseStarted { phase: "explore" });
        let report = explore(&input.question, root, &profile, &cfg)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        progress.emit(PhaseEvent::PhaseFinished { phase: "explore" });

        Ok(CallToolResult::success(vec![Content::text(report)]))
    }

    #[tool(
        description = "Answer a question about a codebase with a capable model, \
            grounded in optional supplied context (typically an `explore` report or \
            pasted source). With context, the model treats it as primary evidence and \
            uses a read-only kaish shell to verify or fill precise gaps; without \
            context, it investigates directly. This is the synthesizer seam on its \
            own — a real outside opinion you can seed with material `explore` or you \
            gathered. Read-only. Args: question (required), context (optional), path \
            (project dir; optional with a default root), provider \
            (anthropic|deepseek|gemini|openai), and an optional model override."
    )]
    async fn synthesize(
        &self,
        Parameters(input): Parameters<SynthesizeInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let mut profile = self.resolve_profile(input.provider)?;
        if let Some(m) = input.model {
            profile.synth_model = m;
        }
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let cfg = ConsultConfig {
            explorer_max_turns: defaults.explorer_max_turns,
            synth_max_turns: defaults.synth_max_turns,
            max_tokens: profile.max_tokens,
            sandbox: self.config.sandbox.clone(),
            progress: progress.clone(),
        };

        progress.emit(PhaseEvent::PhaseStarted { phase: "synthesize" });
        let answer = synthesize(&input.question, input.context.as_deref(), root, &profile, &cfg)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        progress.emit(PhaseEvent::PhaseFinished { phase: "synthesize" });

        Ok(CallToolResult::success(vec![Content::text(answer)]))
    }

    #[tool(
        description = "Run a kaish (sh-like) script against the read-only project; \
            returns exit code + stdout + stderr. Browse code with line numbers: \
            `cat -n FILE`, `rg -n PATTERN`, `cat -n FILE | sed -n '40,80p'`; compose \
            builtins with pipes (grep/jq/awk/find/...). Read-only: writes and external \
            commands are refused (exit 126 = blocked by the sandbox; a script killed \
            for running too long exits 124). Each call starts at the project root — \
            there is no persistent cwd. Learn more kaish without spending a turn: the \
            `kaibo://kaish/*` resources (syntax, builtins, vfs, scatter, …) or run \
            `help`/`help syntax`/`help <builtin>` in the script itself. Args: script \
            (required), path (project dir; optional if the server has a default root)."
    )]
    pub async fn run_kaish(
        &self,
        Parameters(input): Parameters<RunKaishInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;

        // A fresh worker (and kernel) per call: stateless, starts at root, and the
        // !Send kernel stays on its own thread so this future stays Send. Applies the
        // configured sandbox limits (timeout, output cap, disabled builtins).
        let worker = KaishWorker::spawn_with(&root, self.config.sandbox.clone())
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let out = worker
            .run(input.script)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format_output(&out))]))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for KaiboHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                // kaibo mirrors its `tracing` logs onto the MCP `notifications/message`
                // channel (see `mcp_log`); advertising `logging` is what lets a client
                // tune the floor with `logging/setLevel`.
                .enable_logging()
                .build(),
            // Identify as kaibo, not rmcp (from_build_env reports the rmcp crate).
            server_info: Implementation {
                name: "kaibo".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("kaibo (解剖)".to_string()),
                description: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(kaibo_instructions(&self.tool_schemas)),
        }
    }

    /// Honor `logging/setLevel`: record the client's chosen floor so the log-drain
    /// task forwards only records at or above it. The default implementation returns
    /// `method_not_found`, which would make our advertised `logging` capability a lie —
    /// this is the half that makes it real.
    async fn set_level(
        &self,
        params: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.apply_log_level(params.level);
        Ok(())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: kaibo_resources(),
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: kaibo_resource_templates(),
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        read_kaibo_resource(&request.uri, &self.tool_schemas)
    }
}

/// A `text/markdown` resource at `uri` with `name`/`description`. Small helper so
/// the listing reads as a table of what kaibo serves.
fn markdown_resource(uri: &str, name: &str, description: &str) -> rmcp::model::Resource {
    RawResource {
        mime_type: Some("text/markdown".to_string()),
        description: Some(description.to_string()),
        ..RawResource::new(uri, name)
    }
    .no_annotation()
}

/// The resources kaibo advertises: its own read-only sandbox doc plus one per kaish
/// help topic (sourced from `kaish-help`'s registry, so the list tracks upstream).
/// Pure (no `self`, no transport) so the dispatch is unit-testable without
/// fabricating a `RequestContext`.
fn kaibo_resources() -> Vec<rmcp::model::Resource> {
    let mut resources = vec![markdown_resource(
        SANDBOX_URI,
        "kaibo read-only sandbox",
        "kaibo's read-only boundary: line-number browsing idioms and the exit-code contract.",
    )];
    for (topic, description) in topics() {
        resources.push(markdown_resource(
            &format!("{KAISH_RES_PREFIX}{topic}"),
            &format!("kaish: {topic}"),
            description,
        ));
    }
    resources
}

/// The URI templates kaibo advertises: per-builtin help, addressed by name.
fn kaibo_resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
    let template = RawResourceTemplate {
        uri_template: BUILTIN_URI_TEMPLATE.to_string(),
        name: "kaish builtin help".to_string(),
        title: None,
        description: Some(
            "Help for a single kaish builtin — parameters and examples. \
             e.g. kaibo://kaish/builtin/rg"
                .to_string(),
        ),
        mime_type: Some("text/markdown".to_string()),
        icons: None,
    };
    vec![template.no_annotation()]
}

/// Render the markdown body for a kaibo resource URI, or `None` if the URI isn't
/// one kaibo serves. Pure and offline-testable; the handler wraps the result.
fn render_resource(uri: &str, schemas: &[ToolSchema]) -> Option<String> {
    if uri == SANDBOX_URI {
        return Some(kaibo_sandbox_doc());
    }
    if let Some(name) = uri.strip_prefix(BUILTIN_PREFIX) {
        // An unregistered builtin is a miss (not-found), not an "unknown topic" stub.
        return render_builtin_help(name, schemas);
    }
    if let Some(topic) = uri.strip_prefix(KAISH_RES_PREFIX) {
        // Only the registry's own topics — anything else falls through to not-found
        // rather than rendering kaish-help's "Unknown topic" body.
        if topics().iter().any(|(t, _)| *t == topic) {
            return Some(render_topic(topic, schemas));
        }
    }
    None
}

/// Read one kaibo resource by URI. An unknown URI is a not-found error, not a
/// silent empty result — a caller asking for the wrong thing should hear so.
fn read_kaibo_resource(uri: &str, schemas: &[ToolSchema]) -> Result<ReadResourceResult, McpError> {
    let body = render_resource(uri, schemas)
        .ok_or_else(|| McpError::resource_not_found(format!("unknown resource: {uri}"), None))?;
    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(body, uri)],
    })
}

/// Assemble the `consult` tool result. The answer is always the text content
/// (unchanged from a bare consult). The explorer's aggregated report — the
/// `explore′` sweeps the driver delegated — rides along as `structured_content`
/// only when the caller set `include_report`, keeping a normal call lean. When
/// requested it is surfaced even if empty: an empty report is the honest signal
/// that the consult read every span itself and delegated no sweep, which is
/// distinct from the caller not asking at all. Pure and offline-testable.
fn consult_result(answer: String, report: String, include_report: bool) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(answer)]);
    if include_report {
        result.structured_content = Some(json!({ "report": report }));
    }
    result
}

/// The MCP token the client attached for progress, if any. Per the spec, progress
/// notifications are sent *only* when the client opted in by putting a
/// `progressToken` in the request `_meta`; absent one, we stay silent. Pure so the
/// opt-in/opt-out decision is testable without a live request.
fn progress_token(meta: &Meta) -> Option<ProgressToken> {
    meta.get_progress_token()
}

/// Render one [`PhaseEvent`] as an MCP progress notification under `token`. `seq` is
/// the monotonically increasing `progress` value the spec requires (it "should
/// increase every time progress is made, even if the total is unknown"); `total`
/// stays `None` because a consult's step count isn't known up front. Pure — the
/// counting and wiring live in [`ProgressReporter`]; this is just the shape.
fn progress_param(token: ProgressToken, seq: u64, event: &PhaseEvent) -> ProgressNotificationParam {
    ProgressNotificationParam {
        progress_token: token,
        progress: seq as f64,
        total: None,
        message: Some(event.message()),
    }
}

/// Pick the sink for one tool call: a live [`ProgressReporter`] when the client
/// asked for progress (sent a token), else [`NullSink`]. Gating at construction
/// means the no-progress path never even allocates a counter or touches the peer.
fn progress_sink(peer: Peer<RoleServer>, meta: &Meta) -> Arc<dyn ProgressSink> {
    match progress_token(meta) {
        Some(token) => Arc::new(ProgressReporter::new(peer, token)),
        None => Arc::new(NullSink),
    }
}

/// Renders [`PhaseEvent`]s onto the MCP wire as `notifications/progress`, holding the
/// peer, the client's progress token, and the monotonic counter the spec wants.
///
/// `emit` is sync (the loop calls it from inside `async` tool calls and must not
/// block on a progress hop), but `notify_progress` is async — so each event is
/// fired on a detached task. Notifications are best-effort: a send that loses the
/// ordering race still carries its own increasing `progress`, so the client can
/// order by it, and a failed send is dropped rather than allowed to sink the call.
#[derive(Clone)]
struct ProgressReporter {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for ProgressReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressReporter").field("token", &self.token).finish_non_exhaustive()
    }
}

impl ProgressReporter {
    fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        Self { peer, token, seq: Arc::new(AtomicU64::new(0)) }
    }
}

impl ProgressSink for ProgressReporter {
    fn emit(&self, event: PhaseEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let param = progress_param(self.token.clone(), seq, &event);
        let peer = self.peer.clone();
        // Fire-and-forget: don't make the loop await a notification it doesn't depend
        // on. A dead transport just drops it.
        tokio::spawn(async move {
            let _ = peer.notify_progress(param).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use rmcp::ServerHandler;

    /// A small stand-in builtin set so resource rendering is offline-testable.
    fn sample_schemas() -> Vec<ToolSchema> {
        vec![
            ToolSchema::new("cat", "Read a file"),
            ToolSchema::new("rg", "Recursive grep"),
        ]
    }

    fn handler() -> KaiboHandler {
        KaiboHandler::new(Config::builtin()).expect("handler builds")
    }

    /// Progress is opt-in: with no `progressToken` in `_meta` we send nothing, so the
    /// sink must be the no-op. (A `consult` with no token is byte-for-byte its old
    /// silent self.)
    #[test]
    fn no_token_means_no_progress_token() {
        assert!(progress_token(&Meta::default()).is_none());
    }

    /// A token in `_meta` is the opt-in — we surface it so a reporter can be built.
    #[test]
    fn a_progress_token_in_meta_is_surfaced() {
        let token = ProgressToken(NumberOrString::Number(7));
        let meta = Meta::with_progress_token(token.clone());
        assert_eq!(progress_token(&meta), Some(token));
    }

    /// The progress payload carries the client's token, a monotonic `progress`, an
    /// unknown `total`, and the event's human line — the shape the spec wants.
    #[test]
    fn progress_param_carries_seq_and_message() {
        let token = ProgressToken(NumberOrString::String("abc".into()));
        let event = PhaseEvent::SweepStarted { question: "where is X?".into() };
        let p = progress_param(token.clone(), 3, &event);
        assert_eq!(p.progress_token, token);
        assert_eq!(p.progress, 3.0);
        assert!(p.total.is_none(), "a consult's step count isn't known up front");
        assert_eq!(p.message.as_deref(), Some("exploring: where is X?"));
    }

    /// The server advertises the `logging` capability — the half of MCP logging the
    /// client sees at initialize. Without it, a client never knows it can `setLevel`.
    #[test]
    fn advertises_the_logging_capability() {
        let info = handler().get_info();
        assert!(
            info.capabilities.logging.is_some(),
            "logging capability must be advertised, got {:?}",
            info.capabilities
        );
    }

    /// `setLevel` actually moves the shared floor the drain reads. Starts at the
    /// default (info); raising it to `error` stores the higher rank.
    #[test]
    fn set_level_updates_the_shared_floor() {
        let h = handler();
        assert_eq!(
            h.mcp_log_level().load(Ordering::Relaxed),
            mcp_log::rank(mcp_log::DEFAULT_LEVEL),
            "the floor starts at the default level"
        );
        h.apply_log_level(LoggingLevel::Error);
        assert_eq!(
            h.mcp_log_level().load(Ordering::Relaxed),
            mcp_log::rank(LoggingLevel::Error),
            "setLevel must move the floor the drain task reads"
        );
    }

    #[test]
    fn lists_the_sandbox_doc_and_every_kaish_topic() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.raw.uri).collect();
        assert!(
            uris.iter().any(|u| u == SANDBOX_URI),
            "must advertise the sandbox doc, got {uris:?}"
        );
        for (topic, _) in topics() {
            let want = format!("{KAISH_RES_PREFIX}{topic}");
            assert!(
                uris.contains(&want),
                "must advertise the {topic:?} topic at {want}, got {uris:?}"
            );
        }
    }

    #[test]
    fn advertises_the_per_builtin_template() {
        let templates = kaibo_resource_templates();
        assert!(
            templates.iter().any(|t| t.raw.uri_template == BUILTIN_URI_TEMPLATE),
            "must advertise the per-builtin URI template"
        );
    }

    fn read_text(uri: &str, schemas: &[ToolSchema]) -> String {
        let result = read_kaibo_resource(uri, schemas).expect("known uri must read");
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    #[test]
    fn reads_the_sandbox_doc_with_the_idioms_and_codes() {
        let text = read_text(SANDBOX_URI, &[]);
        for needle in ["cat -n", "rg", "read-only", "126", "124"] {
            assert!(text.contains(needle), "sandbox doc must mention {needle:?}");
        }
    }

    #[test]
    fn reads_a_topic_resource() {
        let text = read_text(&format!("{KAISH_RES_PREFIX}syntax"), &[]);
        assert!(text.contains("Variables"), "syntax topic should cover Variables:\n{text}");
    }

    #[test]
    fn reads_a_builtin_resource_and_rejects_an_unknown_builtin() {
        let schemas = sample_schemas();
        let text = read_text(&format!("{BUILTIN_PREFIX}rg"), &schemas);
        assert!(text.contains("rg"), "builtin help should name the tool:\n{text}");
        assert!(
            read_kaibo_resource(&format!("{BUILTIN_PREFIX}nope"), &schemas).is_err(),
            "an unregistered builtin must be a not-found error"
        );
    }

    #[test]
    fn unknown_resource_uri_is_an_error() {
        assert!(
            read_kaibo_resource("kaibo://nope", &[]).is_err(),
            "an unknown URI must be a not-found error, not an empty success"
        );
    }

    /// The text channel of a result (the answer). Panics if it isn't a single
    /// text block, which is the only shape `consult_result` produces.
    fn answer_text(result: &CallToolResult) -> String {
        assert_eq!(result.content.len(), 1, "consult result is a single text block");
        result.content[0]
            .as_text()
            .expect("consult answer is text content")
            .text
            .clone()
    }

    /// Default path: no report requested ⇒ the answer is the whole result and no
    /// structured content rides along (a lean call, byte-for-byte its pre-flag shape).
    #[test]
    fn consult_result_omits_report_unless_requested() {
        let result = consult_result("the answer".into(), "FILE:1 evidence".into(), false);
        assert_eq!(answer_text(&result), "the answer");
        assert!(
            result.structured_content.is_none(),
            "report must not leak into a default call: {:?}",
            result.structured_content
        );
    }

    /// Opt-in: the report is surfaced as structured_content under `report`, while the
    /// answer stays the text channel — the report rides *separately*, not duplicated
    /// into the answer the model reads.
    #[test]
    fn consult_result_attaches_report_when_requested() {
        let result = consult_result("ans".into(), "src/x.rs:1 the snippet".into(), true);
        assert_eq!(answer_text(&result), "ans", "answer stays the text channel");
        assert!(
            !answer_text(&result).contains("the snippet"),
            "report must not be folded into the answer text"
        );
        let sc = result.structured_content.expect("report was requested");
        assert_eq!(sc["report"], "src/x.rs:1 the snippet", "report rides under `report`");
    }

    /// Opt-in with an empty report (the consult delegated no sweep): still surfaced.
    /// Emptiness is the signal — present-but-empty means "asked, no sweep happened",
    /// which a caller must be able to tell apart from "never asked" (None).
    #[test]
    fn consult_result_surfaces_empty_report_when_requested() {
        let result = consult_result("ans".into(), String::new(), true);
        let sc = result.structured_content.expect("requested even when empty");
        assert_eq!(sc["report"], "", "an empty report is surfaced honestly");
    }

    #[test]
    fn instructions_compose_the_canonical_onboarding_and_point_at_resources() {
        let text = kaibo_instructions(&sample_schemas());
        assert!(text.contains("kaibo"), "instructions should introduce kaibo");
        // The canonical onboarding spine from kaish-help.
        assert!(
            text.contains("How kaish works"),
            "instructions should embed the kaish-help foundations:\n{text}"
        );
        // The progressive-learning pointer.
        assert!(
            text.contains("kaibo://kaish/"),
            "instructions should point at the kaish resources"
        );
    }
}
