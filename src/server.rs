//! The MCP server surface: one `consult` tool over the two-phase pipeline.
//!
//! stdio only — like kaish-mcp, kaibo must never bind a socket: it can read a
//! user's filesystem, so the transport pipe is the security boundary.

use std::path::PathBuf;

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
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
use crate::kaish_syntax::{
    kaibo_instructions_with_scope, kaibo_sandbox_doc, render_builtin_help, render_topic, topics,
};
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
/// The resolved runtime configuration: allowed paths, default provider, gated tools,
/// sandbox limits, and profiles with their kind and key sources (never key values).
const CONFIG_URI: &str = "kaibo://config";

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
    /// launched with a default `--root`. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
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
    /// with a default `--root`. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
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
    /// with a default `--root`. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
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
    /// with a default `--root`. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
    /// Each call starts at this root — there is no persistent cwd across calls.
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
    /// The canonicalized allowed path trees. A per-call path must canonicalize to
    /// at-or-under one of these. Computed once at construction from config.root and
    /// config.allow_paths; falls back to the canonicalized cwd when both are empty.
    /// `Arc` because rmcp clones the handler per request.
    allowed_set: Arc<Vec<PathBuf>>,
}

#[tool_router]
impl KaiboHandler {
    /// Build the handler from a resolved [`Config`]. Snapshots the kernel's builtin
    /// schemas up front (a cheap in-memory kernel); a failure here is a broken build,
    /// surfaced at startup rather than papered over with an empty help surface.
    ///
    /// Computes the canonicalized allowed set here so containment is structural: every
    /// tool call routes through `resolve_root`, which checks this set. A nonexistent
    /// or non-directory entry in root / allow_paths is a loud construction error —
    /// a path that can't be canonicalized can't bound anything.
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

        // Build the canonicalized allowed set. Each entry must be canonicalized now
        // so `resolve_root`'s Path::starts_with check is sound (symlinks resolved,
        // `..` collapsed). A nonexistent path can't bound anything — loud error.
        let mut allowed: Vec<PathBuf> = Vec::new();
        if let Some(root) = &config.root {
            let canon = std::fs::canonicalize(root)
                .with_context(|| format!("canonicalizing --root {}", root.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--root {} is not a directory", canon.display());
            }
            allowed.push(canon);
        }
        for p in &config.allow_paths {
            let canon = std::fs::canonicalize(p)
                .with_context(|| format!("canonicalizing --allow-path {}", p.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--allow-path {} is not a directory", canon.display());
            }
            allowed.push(canon);
        }
        // When no root and no allow_paths are given, fall back to the launch cwd.
        // MCP clients start stdio servers with cwd = workspace, so the zero-config
        // case scopes itself to the project naturally.
        if allowed.is_empty() {
            let cwd = std::env::current_dir()
                .context("could not determine current directory for default allowed set")?;
            let canon = std::fs::canonicalize(&cwd)
                .with_context(|| format!("canonicalizing cwd {}", cwd.display()))?;
            allowed.push(canon);
        }

        let sessions = SessionStore::new(config.defaults.session_capacity);
        Ok(Self {
            config: Arc::new(config),
            tool_router,
            tool_schemas: Arc::new(builtin_schemas()?),
            sessions,
            mcp_log_level: Arc::new(AtomicU8::new(mcp_log::rank(mcp_log::DEFAULT_LEVEL))),
            allowed_set: Arc::new(allowed),
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

    /// The canonicalized allowed path trees for this handler. Every tool call's
    /// resolved path must be at-or-under one of these. Exposed for tests and for
    /// startup logging / the `kaibo://config` resource.
    pub fn allowed_set(&self) -> Vec<PathBuf> {
        (*self.allowed_set).clone()
    }

    /// Resolve a call's project root with containment enforcement:
    ///
    /// 1. Select the raw path: the explicit `path` arg, else the server's `--root`.
    ///    An omitted `path` with no `--root` is a parameter error — not a silent
    ///    default (containment does not change this existing behavior).
    /// 2. Canonicalize the selected path (resolves symlinks and `..`). A path that
    ///    doesn't exist is `invalid_params` with the canonicalize error.
    /// 3. Require the canonicalized path to be at-or-under one of the allowed trees.
    ///    A violation is `invalid_params` naming the allowed trees and the three
    ///    widening knobs (`--allow-path`, `KAIBO_ALLOW_PATHS`, `[server] allow_paths`).
    ///
    /// Returns the CANONICALIZED path so the kaish mount target is always resolved.
    fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        // Step 1: select the raw path.
        let raw = match path {
            Some(p) => PathBuf::from(p),
            None => self.config.root.clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no `path` provided and the server has no default --root",
                    None,
                )
            })?,
        };

        // Step 2: canonicalize — resolves symlinks and `..` so starts_with is sound.
        let canon = std::fs::canonicalize(&raw).map_err(|e| {
            McpError::invalid_params(
                format!("path {} could not be resolved: {e}", raw.display()),
                None,
            )
        })?;

        // Step 2b: require a directory, symmetric with the construction-time check on
        // --root and --allow-path entries. A file path passes canonicalization and
        // containment but makes a degenerate session (cwd is a file); reject it here
        // at the parameter boundary with a clear error rather than failing deep in kaish.
        if !canon.is_dir() {
            return Err(McpError::invalid_params(
                format!("path {} is not a directory", canon.display()),
                None,
            ));
        }

        // Step 3: containment check — must be at-or-under an allowed tree.
        let allowed = &self.allowed_set;
        if allowed.iter().any(|tree| canon.starts_with(tree)) {
            return Ok(canon);
        }

        // Violation: name the allowed set and the three widening knobs.
        let trees: Vec<String> = allowed.iter().map(|p| p.display().to_string()).collect();
        Err(McpError::invalid_params(
            format!(
                "path {} resolves to {}, which is outside the allowed set [{}]. \
                 To widen the boundary: pass --allow-path DIR on the command line, \
                 set KAIBO_ALLOW_PATHS=DIR (colon-separated), or add \
                 `[server] allow_paths = [\"DIR\"]` in config.toml.",
                raw.display(),
                canon.display(),
                trees.join(", "),
            ),
            None,
        ))
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
            instructions: Some(kaibo_instructions_with_scope(
                &self.tool_schemas,
                &self.config,
                &self.allowed_set,
            )),
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
        read_kaibo_resource_with_config(
            &request.uri,
            &self.tool_schemas,
            &self.config,
            &self.allowed_set,
        )
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

/// The resources kaibo advertises: the runtime config, the read-only sandbox doc,
/// and one per kaish help topic (sourced from `kaish-help`'s registry, so the list
/// tracks upstream). Pure (no `self`, no transport) so the dispatch is unit-testable
/// without fabricating a `RequestContext`.
fn kaibo_resources() -> Vec<rmcp::model::Resource> {
    let mut resources = vec![
        // The resolved runtime config: allowed paths, default provider, gated tools,
        // sandbox limits, and each profile with its kind, models, and key sources
        // (never key values). Read this to understand the server's current posture.
        RawResource {
            mime_type: Some("application/toml".to_string()),
            description: Some(
                "kaibo's resolved runtime configuration: allowed path trees, default \
                 provider, gated tools, sandbox limits, and each provider profile with \
                 its kind, models, and key sources. Read this to understand the server's \
                 current posture before making calls."
                    .to_string(),
            ),
            ..RawResource::new(CONFIG_URI, "kaibo: runtime config")
        }
        .no_annotation(),
        markdown_resource(
            SANDBOX_URI,
            "kaibo read-only sandbox",
            "kaibo's read-only boundary: line-number browsing idioms and the exit-code contract.",
        ),
    ];
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

/// Render the `kaibo://config` TOML document. Shows the resolved runtime state —
/// allowed trees, default provider, gated tools, sandbox limits, each profile's
/// kind/models/key sources — so a calling model or operator sees the server's
/// current posture at a glance.
///
/// SECRET-SAFETY CONTRACT: this function renders key SOURCE metadata (env var names,
/// key file paths — the operator-configured pointers) but NEVER the resolved key
/// values. The profile struct stores sources, not secrets; this renderer reads only
/// those source fields. If Config ever gains a resolved-key cache, do not read it here.
/// Tests in this file assert the contract holds.
fn render_config_resource(config: &Config, allowed_set: &[PathBuf]) -> String {
    use serde::Serialize;
    use std::collections::BTreeMap;

    // Dedicated render-only shapes — plain Serialize structs that carry exactly what
    // the resource must expose and nothing more. Keeps the contract explicit.

    #[derive(Serialize)]
    struct ConfigDoc {
        /// Allowed path trees: a per-call path must be at-or-under one of these.
        allowed_paths: Vec<String>,
        /// Default root (the --root value), if set.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_root: Option<String>,
        /// Default provider/profile name.
        default_provider: String,
        /// Which tools are currently advertised.
        tools: ToolsDoc,
        /// Read-only sandbox limits.
        sandbox: SandboxDoc,
        /// Provider profiles: kind, models, key sources (never key values).
        profiles: BTreeMap<String, ProfileDoc>,
    }

    #[derive(Serialize)]
    struct ToolsDoc {
        consult: bool,
        explore: bool,
        synthesize: bool,
        run_kaish: bool,
    }

    #[derive(Serialize)]
    struct SandboxDoc {
        exec_timeout_secs: u64,
        output_limit_bytes: usize,
        /// Builtins shadow-blocked beyond the structural read-only guards.
        disable_builtins: Vec<String>,
    }

    #[derive(Serialize)]
    struct ProfileDoc {
        kind: String,
        explorer_model: String,
        synth_model: String,
        max_tokens: u64,
        thinking_budget: u64,
        explorer_temperature: f64,
        synth_temperature: f64,
        top_p: f64,
        /// Env var name whose value is the API key (checked first). The NAME, not
        /// the value — the operator configured this pointer.
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key_env: Option<String>,
        /// Key file path as configured (`~` unexpanded; expansion happens at
        /// key-resolution time). Used when the env var is unset/blank.
        /// The PATH, not its contents.
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key_file: Option<String>,
        /// True when a missing key falls back to a placeholder (keyless endpoint).
        key_optional: bool,
        /// Resolved endpoint for openai-kind profiles. Shows the effective URL the
        /// client will dial (explicit base_url, else OPENAI_BASE_URL, else the
        /// built-in default), matching the "resolved runtime state" promise.
        /// Only present for openai-kind profiles.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        explorer_effort: String,
        synth_effort: String,
        thinking_style: String,
        request_timeout_secs: u64,
    }

    let profiles: BTreeMap<String, ProfileDoc> = config
        .profiles
        .iter()
        .map(|(name, p)| {
            // Exhaustive destructure — any new Profile field is a compile error here,
            // forcing an explicit render-or-skip decision (including the secret-safety
            // review for any field that might resolve a key value).
            let Profile {
                name: _,
                kind,
                base_url,
                api_key_env,
                api_key_file,
                key_optional,
                explorer_model,
                synth_model,
                max_tokens,
                thinking_budget,
                explorer_temperature,
                synth_temperature,
                top_p,
                explorer_effort,
                synth_effort,
                thinking_style,
                request_timeout,
            } = p;
            // For openai-kind profiles, render the resolved endpoint URL so the
            // resource matches its "resolved runtime state" claim. Other kinds have
            // fixed endpoints baked into rig — no operator-relevant base_url to show.
            let rendered_base_url = if *kind == crate::credentials::ProviderKind::Openai {
                Some(p.resolved_base_url())
            } else {
                base_url.clone()
            };
            let doc = ProfileDoc {
                kind: format!("{:?}", kind).to_lowercase(),
                explorer_model: explorer_model.clone(),
                synth_model: synth_model.clone(),
                max_tokens: *max_tokens,
                thinking_budget: *thinking_budget,
                explorer_temperature: *explorer_temperature,
                synth_temperature: *synth_temperature,
                top_p: *top_p,
                // KEY SOURCE ONLY — env var name or file path, never the value.
                api_key_env: api_key_env.clone(),
                api_key_file: api_key_file.clone(),
                key_optional: *key_optional,
                base_url: rendered_base_url,
                explorer_effort: explorer_effort.clone(),
                synth_effort: synth_effort.clone(),
                thinking_style: format!("{:?}", thinking_style).to_lowercase(),
                request_timeout_secs: request_timeout.as_secs(),
            };
            (name.clone(), doc)
        })
        .collect();

    let doc = ConfigDoc {
        allowed_paths: allowed_set.iter().map(|p| p.display().to_string()).collect(),
        default_root: config.root.as_ref().map(|p| p.display().to_string()),
        default_provider: config.default_provider.clone(),
        tools: ToolsDoc {
            consult: config.tools.consult,
            explore: config.tools.explore,
            synthesize: config.tools.synthesize,
            run_kaish: config.tools.run_kaish,
        },
        sandbox: SandboxDoc {
            exec_timeout_secs: config.sandbox.exec_timeout.as_secs(),
            output_limit_bytes: config.sandbox.output_limit_bytes,
            disable_builtins: config.sandbox.disable_builtins.clone(),
        },
        profiles,
    };

    // Serialize to TOML. If the TOML serializer rejects something (unlikely given
    // all fields are primitive strings/ints/bools), crash loudly rather than return
    // a silently truncated or misleading document — the caller would get a half-truth.
    let body = toml::to_string_pretty(&doc)
        .expect("config render structs are TOML-serializable; if this panics, a field type changed");
    // Prepend a comment block that explains how to widen the allowed set — the tool
    // descriptions promise kaibo://config tells a caller how to do this.
    format!(
        "# kaibo resolved runtime configuration\n\
         # To widen the allowed path set:\n\
         #   CLI:    --allow-path DIR  (repeatable)\n\
         #   env:    KAIBO_ALLOW_PATHS=DIR:DIR2  (colon-separated)\n\
         #   config: [server] allow_paths = [\"DIR\"] in config.toml\n\
         # A non-empty --allow-path list replaces the env/file layer.\n\n\
         {body}"
    )
}

/// Read one kaibo resource by URI, with the runtime config and allowed set threaded
/// in for `kaibo://config`. The pure path (kaish/*, sandbox) routes through
/// `render_resource` (line below); the config arm renders via `render_config_resource`.
///
/// This is the handler-level dispatch: call it from `read_resource` so the config
/// resource gets its config.
fn read_kaibo_resource_with_config(
    uri: &str,
    schemas: &[ToolSchema],
    config: &Config,
    allowed_set: &[PathBuf],
) -> Result<ReadResourceResult, McpError> {
    if uri == CONFIG_URI {
        let body = render_config_resource(config, allowed_set);
        return Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(body, uri)],
        });
    }
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
        // Use the config-aware dispatch for all URIs — same path the handler takes.
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        let result = read_kaibo_resource_with_config(uri, schemas, &config, &allowed)
            .expect("known uri must read");
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
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        assert!(
            read_kaibo_resource_with_config(
                &format!("{BUILTIN_PREFIX}nope"),
                &schemas,
                &config,
                &allowed
            )
            .is_err(),
            "an unregistered builtin must be a not-found error"
        );
    }

    #[test]
    fn unknown_resource_uri_is_an_error() {
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        assert!(
            read_kaibo_resource_with_config("kaibo://nope", &[], &config, &allowed).is_err(),
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
        use crate::kaish_syntax::kaibo_instructions;
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

    // --- kaibo://config resource tests ---------------------------------------

    /// The config resource must appear in the listing with the correct URI and a
    /// useful description. Failing until `kaibo://config` is added to
    /// `kaibo_resources()`.
    #[test]
    fn config_resource_is_listed() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.raw.uri).collect();
        assert!(
            uris.iter().any(|u| u == CONFIG_URI),
            "kaibo_resources() must list kaibo://config, got {uris:?}"
        );
        // The resource entry for the config must also have a description.
        let config_res = kaibo_resources()
            .into_iter()
            .find(|r| r.raw.uri == CONFIG_URI)
            .expect("config resource must be listed");
        assert!(
            config_res.raw.description.is_some(),
            "kaibo://config resource must have a description"
        );
    }

    /// The config resource body must contain the key structural fields a calling
    /// model or operator expects: allowed paths, default_provider, gated tools,
    /// sandbox limits, and profiles with their kind and key sources.
    #[test]
    fn config_resource_renders_expected_fields() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/test-allowed")];
        let body = render_config_resource(&config, &allowed);
        // Structural presence checks — the resource is TOML or a document, not prose.
        for needle in ["allowed_paths", "default_provider", "tools", "sandbox", "profiles"] {
            assert!(
                body.contains(needle),
                "config resource must contain {needle:?}:\n{body}"
            );
        }
        // The allowed path we passed must appear.
        assert!(
            body.contains("/tmp/test-allowed"),
            "config resource must show the allowed set:\n{body}"
        );
        // Profiles include the built-in four.
        for profile_name in ["anthropic", "deepseek", "gemini", "openai"] {
            assert!(
                body.contains(profile_name),
                "config resource must list the {profile_name} profile:\n{body}"
            );
        }
        // Key SOURCES (env var name / file path) must appear — operators configured
        // them and need to see them for diagnostics.
        assert!(
            body.contains("ANTHROPIC_API_KEY") || body.contains("api_key_env"),
            "config resource must show key source env var names:\n{body}"
        );
    }

    /// SECRET-SAFETY: the config resource must expose key SOURCE metadata (env var
    /// names, file paths), but NEVER the resolved key values.  We set a sentinel in
    /// the environment and in a temp file, render the resource, and assert the
    /// sentinel appears nowhere in the output.
    ///
    /// `set_var`/`remove_var` are UB when other threads call `getenv` concurrently
    /// (glibc). A mutex serializes the env-touching half against any sibling unit
    /// test in this binary that touches env (there are none today, but the lock is
    /// cheap and structural). The file half needs no mutex.
    #[test]
    fn config_resource_never_exposes_key_values() {
        use std::io::Write;
        use std::sync::Mutex;
        const SENTINEL: &str = "SUPER_SECRET_KEY_VALUE_12345_CANARY";
        // Module-level lock — serializes all set_var/remove_var in this test binary.
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        let var_name = "KAIBO_TEST_SECRET_ENV_VAR_CANARY";
        let allowed = vec![std::path::PathBuf::from("/tmp")];

        // Build the config outside the lock (no env access yet).
        let toml = format!("[profiles.anthropic]\napi_key_env = \"{var_name}\"\n");
        let config = Config::from_toml_str(&toml).expect("valid config");

        // Set the sentinel in env and render inside the lock.
        let body = {
            let _guard = ENV_LOCK.lock().unwrap();
            // SAFETY: holding the lock means no other test in this binary mutates env.
            #[allow(deprecated)]
            unsafe {
                std::env::set_var(var_name, SENTINEL);
            }
            let b = render_config_resource(&config, &allowed);
            #[allow(deprecated)]
            unsafe {
                std::env::remove_var(var_name);
            }
            b
        };

        // The env var *name* must appear (operator needs to see what's configured).
        assert!(
            body.contains(var_name),
            "config resource must show the env var name (not value):\n{body}"
        );
        // The sentinel value must NOT appear — this is the invariant.
        assert!(
            !body.contains(SENTINEL),
            "config resource must NEVER expose the API key value; \
             sentinel found in:\n{body}"
        );

        // The file half needs no env access — no lock needed.
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "{SENTINEL}").expect("write sentinel");
        let file_path = tmp.path().to_string_lossy().to_string();
        let toml2 = format!("[profiles.anthropic]\napi_key_file = \"{file_path}\"\n");
        let config2 = Config::from_toml_str(&toml2).expect("valid config");
        let body2 = render_config_resource(&config2, &allowed);
        // The file path (source pointer) may appear, but not the file contents.
        assert!(
            !body2.contains(SENTINEL),
            "config resource must NEVER expose key file contents; \
             sentinel found in:\n{body2}"
        );
    }

    /// `read_kaibo_resource` extended: kaibo://config must be readable via the
    /// handler-level path (which threads config + allowed_set through).
    #[test]
    fn read_kaibo_config_resource_is_readable() {
        let config = Config::builtin();
        let allowed = handler().allowed_set();
        let body_str = render_config_resource(&config, &allowed);
        // Sanity: the rendered document has something in it.
        assert!(!body_str.is_empty(), "config resource body must not be empty");
        // The dispatch must not return not-found for this URI.
        let result =
            read_kaibo_resource_with_config(CONFIG_URI, &[], &config, &allowed);
        assert!(result.is_ok(), "kaibo://config must be readable, got {result:?}");
    }

    // --- Scope section in instructions ---------------------------------------

    /// Instructions must include a scope section that names the allowed trees and
    /// points at kaibo://config. Failing until kaibo_instructions_with_scope is
    /// added and get_info wires it in.
    #[test]
    fn instructions_scope_section_names_allowed_paths() {
        let schemas = sample_schemas();
        let allowed = vec![
            std::path::PathBuf::from("/projects/myapp"),
            std::path::PathBuf::from("/data/shared"),
        ];
        let config = Config::builtin();
        let text = kaibo_instructions_with_scope(&schemas, &config, &allowed);
        // The scope section must name each allowed path.
        assert!(
            text.contains("/projects/myapp"),
            "scope section must name the first allowed path:\n{text}"
        );
        assert!(
            text.contains("/data/shared"),
            "scope section must name the second allowed path:\n{text}"
        );
        // Points at the config resource for the full picture.
        assert!(
            text.contains(CONFIG_URI),
            "scope section must mention kaibo://config:\n{text}"
        );
    }

    /// When there is a default root, the scope section must say so rather than
    /// saying "no default root".
    #[test]
    fn instructions_scope_section_names_default_root() {
        let schemas = sample_schemas();
        let mut config = Config::builtin();
        config.root = Some(std::path::PathBuf::from("/projects/myapp"));
        let allowed = vec![std::path::PathBuf::from("/projects/myapp")];
        let text = kaibo_instructions_with_scope(&schemas, &config, &allowed);
        assert!(
            text.contains("/projects/myapp"),
            "scope section must name the configured root:\n{text}"
        );
    }

    /// When no default root is set the scope section must be honest about it.
    #[test]
    fn instructions_scope_section_states_no_default_root_when_absent() {
        let schemas = sample_schemas();
        let mut config = Config::builtin();
        config.root = None;
        let allowed = vec![std::path::PathBuf::from("/tmp")];
        let text = kaibo_instructions_with_scope(&schemas, &config, &allowed);
        // Must explain that every call must pass a path.
        assert!(
            text.to_lowercase().contains("every call") || text.contains("no default"),
            "scope section must note the absence of a default root:\n{text}"
        );
    }
}
