//! The MCP server surface: one `consult` tool over the two-phase pipeline.
//!
//! stdio only — like kaish-mcp, kaibo must never bind a socket: it can read a
//! user's filesystem, so the transport pipe is the security boundary.

use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use kaish_kernel::tools::ToolSchema;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, GetPromptRequestParams, GetPromptResult, Implementation,
    JsonObject, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, LoggingLevel,
    Meta, PaginatedRequestParams, ProgressNotificationParam, ProgressToken, Prompt, PromptArgument,
    PromptMessage, PromptMessageRole, ProtocolVersion, RawResource, RawResourceTemplate,
    ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
    ServerInfo, SetLevelRequestParams,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{Peer, RequestContext};
use rmcp::ErrorData as McpError;
use rmcp::{tool, tool_handler, tool_router, RoleServer};
use serde::Deserialize;
use serde_json::json;
use tracing::Instrument;

use crate::config::{Backend, Cast, Config, ModelRole, ModelSlot};
use crate::consult::{consult, oneshot, Arm, ConsultConfig, ModelCaps, ModelShape, PromptOverrides};
use crate::explorer::format_output;
use crate::generate_image::GenerateImageInput;
use crate::image_gen::ImageGen;
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
/// The resolved runtime configuration: allowed paths, default cast, gated tools,
/// sandbox limits, backends with their key sources (never key values), and casts
/// with their resolved slots.
const CONFIG_URI: &str = "kaibo://config";
/// The annotated config *template* — every knob with its default, commented. The
/// companion to `kaibo://config` (which shows the *resolved* state): this is what a
/// user copies to `~/.config/kaibo/config.toml`. Most useful on a fresh install,
/// where the setup guidance points at it.
const CONFIG_EXAMPLE_URI: &str = "kaibo://config/example";
/// `docs/config.example.toml`, embedded at compile time so it ships *inside* the
/// binary — `cargo install kaibo` lays down no docs, so reading the file at runtime
/// would 404 at exactly the fresh-install moment the example matters most.
const CONFIG_EXAMPLE_TOML: &str = include_str!("../docs/config.example.toml");

/// Which tools to advertise. All on by default; each `--no-<tool>` flips one off.
///
/// Composes to any posture: `{oneshot:false}` ≈ the codebase-only surface; only
/// `run_kaish` on ≈ "no code leaves the box, kaibo as a pure read-only shell". A
/// server with *all* off is a misconfiguration — refused at startup (see `main`),
/// not represented as a valid state here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolGating {
    pub consult: bool,
    pub oneshot: bool,
    pub run_kaish: bool,
    pub generate_image: bool,
    /// The batch capability (submit/get/cancel/list) — one gate over all the verbs:
    /// they're one capability (you can't get or list without submit), so `--no-batch`
    /// drops them together rather than a flag apiece.
    pub batch: bool,
}

impl Default for ToolGating {
    fn default() -> Self {
        Self {
            consult: true,
            oneshot: true,
            run_kaish: true,
            generate_image: true,
            batch: true,
        }
    }
}

impl ToolGating {
    /// True iff every tool is disabled — the zero-tool server we refuse to start.
    pub fn all_disabled(&self) -> bool {
        !self.consult && !self.oneshot && !self.run_kaish && !self.generate_image && !self.batch
    }
}

/// Arguments to the `consult` tool. `deny_unknown_fields` (here and on every tool
/// input): a typo'd or misplaced argument must be a loud invalid-params error —
/// serde would otherwise drop it and the call would run on configured defaults
/// while the caller believes the override applied. Serde aliases stay accepted.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsultInput {
    /// The question to investigate about the project. Say what you did or want to
    /// know in prose — what you changed and why, the behavior you're asking about.
    /// kaibo locates and reads the real, current code itself, so describing your
    /// intent beats pasting a diff or a file dump it would only re-read from disk.
    pub question: String,

    /// Optional context to seed the investigation — a change/diff *summary*, a prior
    /// report, or pasted source kaibo can't reach. It's treated as trusted starting
    /// evidence: kaibo trusts a cited `file:line` rather than re-deriving it, and
    /// spends its turns getting more where the context runs out. Prefer a prose
    /// summary of intent over a raw diff — kaibo reads the current code itself.
    #[serde(default)]
    pub context: Option<String>,

    /// Absolute path to the project to explore. Optional when the server has a
    /// default root — an explicit `--root`, or the launch cwd inferred when it's
    /// inside the allowed set. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
    #[serde(default)]
    pub path: Option<String>,

    /// Which cast (model team) runs this call; omit for the server's default.
    /// Pick from this param's `enum` — the casts live right now; `kaibo://config`
    /// lists every configured cast and backend, with their aliases.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the explorer (investigation) model id. Sent verbatim — an id
    /// containing "/" (HuggingFace-style org/model) is still one id. Keeps the
    /// slot's configured backend unless `explorer_backend` retargets it.
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Run the explorer override on this backend (name or alias) instead of the
    /// slot's configured one. Requires `explorer_model`; together they replace
    /// the slot wholesale, so this also works on a role the cast doesn't carry.
    #[serde(default)]
    pub explorer_backend: Option<String>,

    /// Override the synthesizer (final-answer) model id. Sent verbatim — an id
    /// containing "/" is still one id. Keeps the slot's configured backend
    /// unless `synth_backend` retargets it.
    #[serde(default)]
    pub synth_model: Option<String>,

    /// Run the synth override on this backend (name or alias) instead of the
    /// slot's configured one. Requires `synth_model`; together they replace the
    /// slot wholesale, so this also works on a role the cast doesn't carry.
    #[serde(default)]
    pub synth_backend: Option<String>,

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

/// Arguments to the `oneshot` tool. See [`ConsultInput`] for the
/// `deny_unknown_fields` rationale. No `path`: oneshot reads no project — it's a
/// thin, toolless completion, so the caller owns any context the model needs.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OneshotInput {
    /// The prompt to send the model. There's no codebase access on this call, so
    /// include whatever context the answer needs (pasted source, a spec, the data) —
    /// the model answers from this and its own knowledge, nothing else.
    pub prompt: String,

    /// Workspace files to inline as context for the prompt — absolute or relative paths
    /// under the allowed set (same boundary as `path` elsewhere; worktrees included).
    /// kaibo reads each file and inlines it so its bytes never pass through your context.
    /// Prefer **whole files**: a tool-less model has no way to go read the repo itself, so
    /// give it the full file(s) it needs — `["README.md", "src/server.rs"]` — not a snippet.
    /// Top-tier models carry very large context windows (1M+ tokens), so don't be stingy —
    /// attach whole files, even several, rather than trimming; it has plenty of room to work
    /// with the full source. (A `git diff > changes.diff` then `["changes.diff"]` works for
    /// reviewing *uncommitted* changes, but a diff is leaner context than the files
    /// themselves.) The same surface as `batch_submit`'s `attach`, on the interactive
    /// (synchronous) call. Text files splice in
    /// as text; images (png/jpeg/gif/webp) ride as native image parts (needs a vision-capable
    /// model). A path outside the workspace, a directory, an oversized file, or a binary that
    /// isn't a known image is refused with a clear error. Omit for none.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Which cast (model team) runs this call; omit for the server's default. Pick
    /// from this param's `enum` — the casts live right now; `kaibo://config` lists
    /// every configured cast and backend, with their aliases. kaibo runs the cast's
    /// capable (synth) model.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the model id. Sent verbatim — an id containing "/" is still one id.
    /// Keeps the slot's configured backend unless `backend` retargets it.
    #[serde(default)]
    pub model: Option<String>,

    /// Run the model override on this backend (name or alias) instead of the
    /// slot's configured one. Requires `model`.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments to `batch_submit`. Many prompts, one cast/model — they all ride one
/// provider batch. See [`ConsultInput`] for the `deny_unknown_fields` rationale.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchSubmitInput {
    /// The prompts to fan out, one batch item each. Like `oneshot`, there's no
    /// codebase access — each prompt carries its own context; the model answers from
    /// it and its own knowledge. Batch runs them at max thinking, so it's the lane for
    /// hard questions you're willing to wait on.
    pub prompts: Vec<String>,

    /// Workspace files to inline as shared context for *every* prompt in the batch —
    /// absolute or relative paths under the allowed set (same boundary as `path`
    /// elsewhere; worktrees included). kaibo reads each file and inlines it so its bytes
    /// never pass through your context. Prefer **whole files**: a tool-less model has no way
    /// to go read the repo itself, so give it the full file(s) it needs —
    /// `["README.md", "src/server.rs"]` — not a snippet. Top-tier models carry very large
    /// context windows (1M+ tokens), so don't be stingy — attach whole files, even several,
    /// rather than trimming. (A `git diff > changes.diff` then `["changes.diff"]` works for
    /// reviewing *uncommitted* changes, but a diff is leaner context than the files
    /// themselves.) Text files splice in as text; images
    /// (png/jpeg/gif/webp) ride as native image parts (needs a vision-capable
    /// synth model). A path outside the workspace, a directory, an oversized file, or a
    /// binary that isn't a known image is refused with a clear error. Omit for none.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Which cast (model team) runs the batch; omit for the server's default. Batch
    /// uses the cast's capable (synth) model on a batch-capable backend; `kaibo://config`
    /// lists the casts.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the synth model id. Sent verbatim — an id containing "/" is still one
    /// id. Keeps the cast's backend unless `backend` retargets it. Reach for this to
    /// batch a top-tier model the cast synths something cheaper for interactive use.
    #[serde(default)]
    pub model: Option<String>,

    /// Run the `model` override on this backend (name or alias) instead of the cast's.
    /// Requires `model`. Must be a batch-capable backend.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments to `batch_list`: an optional backend to scope the listing to.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchListInput {
    /// Which backend (name or alias) to list batches from. Omit to list across every
    /// configured batch-capable backend — the orphan-recovery default when you've lost a
    /// handle and don't recall which backend ran it.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments to `batch_get` / `batch_cancel`: the opaque handle `batch_submit`
/// returned (`"backend/provider-id"`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchHandleInput {
    /// The batch handle kaibo returned from `batch_submit` (e.g.
    /// `anthropic/msgbatch_…`). kaibo holds no state — the handle is the whole address.
    pub batch_id: String,
}

/// Arguments to the `run_kaish` tool. See [`ConsultInput`] for the
/// `deny_unknown_fields` rationale.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunKaishInput {
    /// The kaish (sh-like) script to run against the read-only project.
    pub script: String,

    /// Absolute path to the project. Optional when the server has a default root
    /// — an explicit `--root`, or the launch cwd inferred when it's inside the
    /// allowed set. Must be at-or-under an allowed tree; see
    /// `kaibo://config` for the server's current allowed set and how to widen it.
    /// Each call starts at this root — there is no persistent cwd across calls.
    #[serde(default)]
    pub path: Option<String>,
}

/// kaibo's MCP handler. Cheap to clone (rmcp clones it per request).
#[derive(Clone)]
pub struct KaiboHandler {
    /// The resolved configuration: backend + cast registries, defaults, default
    /// root and cast. `Arc` because rmcp clones the handler per request and it's
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
    /// The effective default root a call uses when it omits `path` — the explicit
    /// `--root`/config value, or the launch cwd inferred when it falls inside the
    /// allowed set. Canonicalized. `None` only when no root was configured *and* the
    /// cwd is outside the allowed set (an `--allow-path` that excludes the cwd), in
    /// which case an omitted `path` stays a parameter error. `Arc` for cheap clone.
    default_root: Arc<Option<PathBuf>>,
    /// True when [`Self::default_root`] was inferred from the launch cwd rather than
    /// configured explicitly. Surfaced as "(inferred from launch cwd)" in the scope
    /// section and `kaibo://config` so the boundary stays legible.
    default_root_inferred: bool,
}

/// Inject `casts` as a JSON-Schema `enum` on the `cast` parameter of every
/// consultation tool still in `router` (consult/oneshot — the tools whose `cast`
/// selects the answering team). This surfaces the live roster
/// where an agent actually picks an argument value, instead of deferring it to
/// prose the host may drop.
///
/// Advisory, not enforcing: `call_tool` deserializes the args with serde, which
/// ignores `enum`, so a config-only cast passed by name still resolves — this
/// only advertises the common set. Skipped when `casts` is empty (no cast can
/// reach a model), because an empty `enum` reads as "no valid value" to a strict
/// client and would wrongly forbid the field, which is optional. A gated-off tool
/// is already absent from `router`, so the lookups simply skip it.
fn inject_cast_enum(router: &mut ToolRouter<KaiboHandler>, tools: &[&str], casts: &[String]) {
    if casts.is_empty() {
        return;
    }
    let values: Vec<serde_json::Value> = casts
        .iter()
        .map(|c| serde_json::Value::String(c.clone()))
        .collect();
    for name in tools {
        let Some(route) = router.map.get_mut(*name) else {
            continue;
        };
        let mut schema = (*route.attr.input_schema).clone();
        if let Some(cast) = schema
            .get_mut("properties")
            .and_then(|p| p.as_object_mut())
            .and_then(|props| props.get_mut("cast"))
            .and_then(|c| c.as_object_mut())
        {
            cast.insert("enum".to_string(), serde_json::Value::Array(values.clone()));
            route.attr.input_schema = Arc::new(schema);
        }
    }
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
            (gating.oneshot, "oneshot"),
            (gating.run_kaish, "run_kaish"),
            (gating.generate_image, "generate_image"),
            // One `--no-batch` flag drops all batch routes together.
            (gating.batch, "batch_submit"),
            (gating.batch, "batch_get"),
            (gating.batch, "batch_cancel"),
            (gating.batch, "batch_list"),
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
        // The explicit default root, if `--root` was configured — canonicalized so it
        // doubles as both an allowed tree and the call's default root.
        let mut explicit_root: Option<PathBuf> = None;
        if let Some(root) = &config.root {
            let canon = std::fs::canonicalize(root)
                .with_context(|| format!("canonicalizing --root {}", root.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--root {} is not a directory", canon.display());
            }
            allowed.push(canon.clone());
            explicit_root = Some(canon);
        }
        for p in &config.allow_paths {
            let canon = std::fs::canonicalize(p)
                .with_context(|| format!("canonicalizing --allow-path {}", p.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("--allow-path {} is not a directory", canon.display());
            }
            allowed.push(canon);
        }

        // Resolve the default root and the allowed-set cwd fallback together. With an
        // explicit `--root`, that is the default root and no cwd is consulted. Without
        // one, the launch cwd does double duty: MCP clients start stdio servers with
        // cwd = workspace, so it is both the zero-config allowed tree *and* the natural
        // default root — adopt it as the inferred default root whenever it falls inside
        // the allowed set, so a call may omit `path` in the common single-workspace
        // case. We never adopt a cwd the containment check would then reject (an
        // `--allow-path` that excludes the cwd leaves the default root unset, and an
        // omitted `path` stays a parameter error).
        let (default_root, default_root_inferred): (Option<PathBuf>, bool) = match explicit_root {
            Some(root) => (Some(root), false),
            None => {
                let cwd = std::env::current_dir()
                    .context("could not determine current directory for the default root")?;
                let cwd_canon = std::fs::canonicalize(&cwd)
                    .with_context(|| format!("canonicalizing cwd {}", cwd.display()))?;
                if allowed.is_empty() {
                    // Zero config: the workspace is the whole boundary. Push it here,
                    // *before* the guard below, so the `starts_with` check sees it and
                    // adopts cwd as the default root in the zero-config case.
                    allowed.push(cwd_canon.clone());
                }
                // Mirror `resolve_root`'s containment check (step 3): only adopt cwd
                // when it falls inside the allowed set, so we never default to a path
                // the call-time check would reject.
                if allowed.iter().any(|tree| cwd_canon.starts_with(tree)) {
                    (Some(cwd_canon), true)
                } else {
                    (None, false)
                }
            }
        };

        // Stamp the live cast roster onto the consultation tools' `cast` param as a
        // JSON-Schema `enum`, so an agent choosing a team reads the menu off the tool
        // schema it already fills arguments from — not only the handshake prose, which
        // a host may truncate (the failure that motivated this). Env is read once here,
        // the same startup moment the handshake resolves its list; reconnect re-reads.
        let usable: Vec<String> = config
            .usable_casts(|k| std::env::var(k).ok())
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        inject_cast_enum(&mut tool_router, &["consult", "oneshot"], &usable);
        // `generate_image` selects the `image` slot, not explorer/synth, so its menu is
        // a different filter — casts with a usable image slot, not `usable_casts`. Same
        // advisory enum so image gen is as discoverable as consultation.
        let image_casts = config.image_capable_casts(|k| std::env::var(k).ok());
        inject_cast_enum(&mut tool_router, &["generate_image"], &image_casts);

        let sessions = SessionStore::new(config.defaults.session_capacity);
        Ok(Self {
            config: Arc::new(config),
            tool_router,
            tool_schemas: Arc::new(builtin_schemas()?),
            sessions,
            mcp_log_level: Arc::new(AtomicU8::new(mcp_log::rank(mcp_log::DEFAULT_LEVEL))),
            allowed_set: Arc::new(allowed),
            default_root: Arc::new(default_root),
            default_root_inferred,
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
        self.mcp_log_level
            .store(mcp_log::rank(level), Ordering::Relaxed);
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

    /// The effective default root — what a call resolves to when it omits `path`.
    /// The explicit `--root`/config value, or the launch cwd when it was inferred
    /// (see [`Self::default_root_inferred`]); `None` when neither applies. Exposed
    /// for tests and the `kaibo://config` resource.
    pub fn default_root(&self) -> Option<PathBuf> {
        (*self.default_root).clone()
    }

    /// Whether [`Self::default_root`] was inferred from the launch cwd rather than
    /// configured explicitly.
    pub fn default_root_inferred(&self) -> bool {
        self.default_root_inferred
    }

    /// Resolve a call's project root with containment enforcement:
    ///
    /// 1. Select the raw path: the explicit `path` arg, else the effective default
    ///    root (an explicit `--root`, or the launch cwd inferred when it falls inside
    ///    the allowed set). An omitted `path` with no default root is a parameter
    ///    error — not a silent default.
    /// 2. Canonicalize the selected path (resolves symlinks and `..`). A path that
    ///    doesn't exist is `invalid_params` with the canonicalize error.
    /// 3. Require the canonicalized path to be at-or-under one of the allowed trees.
    ///    A violation is `invalid_params` naming the allowed trees and the three
    ///    widening knobs (`--allow-path`, `KAIBO_ALLOW_PATHS`, `[server] allow_paths`).
    ///
    /// Returns the CANONICALIZED path so the kaish mount target is always resolved.
    fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        // Step 1: select the raw path. The default root is the explicit `--root` or
        // the inferred launch cwd (already canonicalized and dir-checked at startup,
        // and guaranteed inside the allowed set); the steps below re-validate it
        // uniformly with an explicit `path`, so there is no special-casing here.
        let raw = match path {
            Some(p) => PathBuf::from(p),
            None => (*self.default_root).clone().ok_or_else(|| {
                McpError::invalid_params(
                    "no `path` provided and the server has no default root \
                     (configure one with --root, or launch kaibo with its cwd \
                     inside the allowed set so the workspace is inferred)",
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

        // Step 3: containment check — must be at-or-under an allowed tree (or a
        // followed worktree of one). Shared with `resolve_attachments` so a file
        // attachment obeys the exact same boundary as a session root, not a parallel
        // check that could drift.
        if self.contained(&canon) {
            return Ok(canon);
        }
        Err(self.containment_error(&raw, &canon))
    }

    /// Is `canon` (already canonicalized) inside the allowed boundary? At-or-under a
    /// static allowed tree, or — when `follow_worktrees` is on — inside a linked git
    /// worktree that an already-allowed repo vouches for (a sibling branch checkout
    /// reachable without an --allow-path). Worktree membership is resolved by reading
    /// git's link files, never by running git (subprocess/git are compiled out — see
    /// sandbox.rs), and trust flows only outward from the allowed repo (we enumerate
    /// the worktrees its own common git dir names and never consult the candidate's
    /// `.git`), so a foreign dir can't forge its way in. The single containment
    /// predicate — `resolve_root` and `resolve_attachments` both defer to it.
    fn contained(&self, canon: &std::path::Path) -> bool {
        if self.allowed_set.iter().any(|tree| canon.starts_with(tree)) {
            return true;
        }
        self.config.follow_worktrees && self.is_followed_worktree(canon)
    }

    /// The shared "outside the allowed set" rejection, naming the boundary and the three
    /// widening knobs. Used wherever [`contained`](Self::contained) says no, so the
    /// caller always learns where the edge is and how to move it.
    fn containment_error(&self, raw: &std::path::Path, canon: &std::path::Path) -> McpError {
        let trees: Vec<String> = self.allowed_set.iter().map(|p| p.display().to_string()).collect();
        McpError::invalid_params(
            format!(
                "path {} resolves to {}, which is outside the allowed set [{}]. \
                 To widen the boundary: pass --allow-path DIR on the command line, \
                 set KAIBO_ALLOW_PATHS=DIR (colon-separated), or add \
                 `[server] allow_paths = [\"DIR\"]` in config.toml. The config and env \
                 forms expand `$VAR` / `${{VAR}}` and a leading `~`, so a scratch dir \
                 reads portably as `\"$TMPDIR\"` / `\"$XDG_RUNTIME_DIR/...\"`.",
                raw.display(),
                canon.display(),
                trees.join(", "),
            ),
            None,
        )
    }

    /// Refuse image attachments to a vision-blind model, naming the cast so the caller
    /// can pick a vision-capable one. Shared by the tool-less attach surfaces (`batch` and
    /// `oneshot`) so the refusal reads identically and lives in one place; a blind model
    /// would silently ignore or error on an image part, so we refuse honestly up front
    /// (the project posture) rather than ship a no-op image. Text-only attachments (and
    /// the no-attachment case) always pass.
    pub fn gate_image_attachments(
        &self,
        vision: bool,
        attachments: &[crate::attach::Attachment],
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        if !vision
            && attachments
                .iter()
                .any(|a| matches!(a, crate::attach::Attachment::Image { .. }))
        {
            return Err(McpError::invalid_params(
                format!(
                    "an image attachment was given, but the model `{model}` on cast `{cast}` \
                     doesn't accept image input. Use a vision-capable cast/model, or attach \
                     only text files. `kaibo://config` lists each slot's `vision`."
                ),
                None,
            ));
        }
        Ok(())
    }

    /// Resolve caller-named attachment paths into [`Attachment`](crate::attach::Attachment)s,
    /// read and encoded server-side so the bytes never transit the calling agent's
    /// context. Each path obeys the *same* boundary as a session root — canonicalize
    /// (symlinks + `..` resolved), require a regular file, then the shared
    /// [`contained`](Self::contained) check (allowed set + followed worktrees) — so
    /// attachments can't read outside the workspace any more than `run_kaish` can.
    ///
    /// Failures are loud and per-path: a missing file, a directory, an over-cap or
    /// non-text/non-image file is a clear `invalid_params`, never a silent skip — an
    /// attachment the caller named but we dropped would be a corrupt answer. An absolute
    /// size ceiling is enforced *before* reading (via the file's metadata) so a giant
    /// file is refused without first slurping it into memory.
    ///
    /// Containment is checked on the *canonical* path, then the read follows — a
    /// check-then-open TOCTOU window, the same class `resolve_root` carries (and tracked
    /// in `docs/issues.md`). The attacker model makes it narrow: kaibo cannot write the
    /// workspace, so swapping the canonical path for an outside-pointing symlink in the
    /// sub-millisecond window needs a *concurrent* writer to the very workspace the
    /// calling agent owns — a self-attack. Closing it structurally (open `O_NOFOLLOW`,
    /// `fstat` the fd, contain via `/proc/self/fd`, read from the fd) is deferred until
    /// the attacker model justifies it.
    pub fn resolve_attachments(
        &self,
        paths: &[String],
    ) -> Result<Vec<crate::attach::Attachment>, McpError> {
        use crate::attach::{classify, DEFAULT_MAX_IMAGE_BYTES, DEFAULT_MAX_TEXT_BYTES};
        // The pre-read ceiling: whichever encoding cap is larger. `classify` applies the
        // precise per-encoding cap after sniffing; this just bounds the read itself.
        let read_ceiling = DEFAULT_MAX_TEXT_BYTES.max(DEFAULT_MAX_IMAGE_BYTES);
        paths
            .iter()
            .map(|p| {
                let raw = std::path::PathBuf::from(p);
                let canon = std::fs::canonicalize(&raw).map_err(|e| {
                    McpError::invalid_params(
                        format!("attachment {} could not be resolved: {e}", raw.display()),
                        None,
                    )
                })?;
                // A regular file, not a directory — symmetric with resolve_root's dir
                // check, the mirror image (we inline a file's bytes, not mount a tree).
                let meta = std::fs::metadata(&canon).map_err(|e| {
                    McpError::invalid_params(
                        format!("attachment {} could not be read: {e}", canon.display()),
                        None,
                    )
                })?;
                if !meta.is_file() {
                    return Err(McpError::invalid_params(
                        format!("attachment {} is not a regular file", canon.display()),
                        None,
                    ));
                }
                // Same boundary as a session root.
                if !self.contained(&canon) {
                    return Err(self.containment_error(&raw, &canon));
                }
                // Bound the read by the absolute ceiling before slurping.
                if meta.len() > read_ceiling as u64 {
                    return Err(McpError::invalid_params(
                        format!(
                            "attachment {} is {} bytes, over the {read_ceiling}-byte limit",
                            canon.display(),
                            meta.len()
                        ),
                        None,
                    ));
                }
                let bytes = std::fs::read(&canon).map_err(|e| {
                    McpError::invalid_params(
                        format!("attachment {} could not be read: {e}", canon.display()),
                        None,
                    )
                })?;
                // Label the attachment with the caller's path (what they typed), not the
                // canonical one — it's their reference and it's what the model should see.
                classify(p, &bytes, DEFAULT_MAX_TEXT_BYTES, DEFAULT_MAX_IMAGE_BYTES)
                    .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))
            })
            .collect()
    }

    /// True when `canon` falls inside a git worktree that an already-allowed repo
    /// vouches for. The trusted side does the vouching: for each allowed tree we
    /// resolve *its* common git dir and enumerate the worktrees that common dir
    /// names; `canon` is admitted only if it sits inside one. We never read
    /// `canon`'s own `.git`, so a forged pointer there can't smuggle a foreign path
    /// in. Pure file reads on the (rare) containment-miss path — see
    /// [`crate::worktree`].
    fn is_followed_worktree(&self, canon: &std::path::Path) -> bool {
        self.allowed_set.iter().any(|tree| {
            crate::worktree::common_git_dir(tree)
                .map(|common| {
                    crate::worktree::vouched_worktrees(&common)
                        .iter()
                        .any(|wt| canon.starts_with(wt))
                })
                .unwrap_or(false)
        })
    }

    /// The worktrees the follow feature currently admits *beyond* the static allowed
    /// set — for the `kaibo://config` runtime section, so the live boundary stays
    /// legible. Recomputed on each read (it reflects worktrees that exist now, which
    /// can change between calls). Empty when the feature is off or nothing extra is
    /// reachable. Deduplicated and sorted for a stable resource.
    fn followed_worktrees(&self) -> Vec<PathBuf> {
        if !self.config.follow_worktrees {
            return Vec::new();
        }
        let mut found: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for tree in self.allowed_set.iter() {
            let Some(common) = crate::worktree::common_git_dir(tree) else {
                continue;
            };
            for wt in crate::worktree::vouched_worktrees(&common) {
                // Skip worktrees already covered by the static set — those aren't
                // "extra"; the runtime section reports only what follow adds.
                if !self.allowed_set.iter().any(|t| wt.starts_with(t)) {
                    found.insert(wt);
                }
            }
        }
        found.into_iter().collect()
    }

    /// Assemble the operator's house rules for this call against the resolved
    /// `root`: the `[context]` files read here, in trusted server-side Rust, and
    /// folded into the phase preamble. A missing *declared* user file is a loud
    /// `internal_error`, never a silent skip — the operator was counting on that
    /// guidance reaching the model. `None` when nothing's configured/present.
    fn house_rules(&self, root: &std::path::Path) -> Result<Option<Arc<str>>, McpError> {
        self.config
            .context
            .assemble(root)
            .map(|opt| opt.map(Arc::from))
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Assemble the static repo-orientation map for this call against the resolved
    /// `root` — the `[orientation]` block injected into the exploring preamble. Runs
    /// the kernel's own `glob` server-side (no model turn). A repo over the file
    /// ceiling is a loud `internal_error` (the operator chose that limit), per
    /// `OrientationConfig::assemble`. Only the *exploring* tools call this.
    async fn orientation(&self, root: &std::path::Path) -> Result<Option<Arc<str>>, McpError> {
        self.config
            .orientation
            .assemble(root, self.config.sandbox.clone())
            .await
            .map(|opt| opt.map(Arc::from))
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Resolve this call's per-phase system prompts for `cast`: the per-model slot
    /// `preamble` (if set) wins over the global `[prompts].<phase>`, which wins over
    /// the built-in (resolved downstream in `consult.rs`). The synth slot's preamble
    /// feeds *both* capable-model tools — the `consult` driver and the toolless
    /// `oneshot` — but through their own keys, so they stay independently overridable
    /// (a copy today, free to diverge). `cast` is the post-override clone, so a
    /// per-call model override (a bare slot) correctly carries no preamble.
    fn resolved_prompts(&self, cast: &Cast) -> PromptOverrides {
        let mut p = self.config.prompts.clone();
        if let Some(pre) = cast
            .slot(ModelRole::Explorer)
            .and_then(|s| s.preamble.clone())
        {
            p.explorer = Some(pre);
        }
        if let Some(pre) = cast.slot(ModelRole::Synth).and_then(|s| s.preamble.clone()) {
            p.consult = Some(pre.clone());
            p.oneshot = Some(pre);
        }
        p
    }

    /// Resolve a call's cast: the explicit name (looked up in the registry, by
    /// name or alias), else the server's default cast. An unknown name is a
    /// parameter error naming the available casts. Returns an owned clone so the
    /// caller can layer per-call model overrides onto it.
    fn resolve_cast(&self, cast: Option<String>) -> Result<Cast, McpError> {
        let name = cast.unwrap_or_else(|| self.config.default_cast.clone());
        self.config
            .resolve_cast(&name)
            .cloned()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    /// Apply a per-call model override to one of `cast`'s slots.
    ///
    /// The model id rides *verbatim* — an id containing `/` (HuggingFace-style
    /// `org/model`) is still one id, never parsed for a backend, so an org prefix
    /// that happens to match a backend alias (`google/…`, `gemma/…`) can never
    /// silently retarget the call. Retargeting is the explicit `backend` arg's
    /// job: when set, it resolves (aliases included) and the slot is replaced
    /// wholesale, which also works on a role the cast doesn't carry. Either way
    /// the configured slot's pins/tunables are dropped — they described the
    /// *configured* model; the new id classifies fresh.
    fn override_model(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: &str,
        backend: Option<&str>,
    ) -> Result<(), McpError> {
        let model = model.trim();
        if model.is_empty() {
            // Same loud rule config load applies to slots (config.rs): an empty
            // model id is a typo, never an intent — it would otherwise surface
            // as a baffling provider 404 mid-call.
            return Err(McpError::invalid_params(
                format!("the {} model id is empty", role.key()),
                None,
            ));
        }
        let backend = match backend {
            Some(name) => self
                .config
                .resolve_backend(name)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?
                .name
                .clone(),
            None => cast.slot(role).map(|s| s.backend.clone()).ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "cast {:?} has no {} slot to override — pass the matching \
                         backend override arg to target one",
                        cast.name,
                        role.key()
                    ),
                    None,
                )
            })?,
        };
        cast.slots.insert(role, ModelSlot::bare(backend, model));
        Ok(())
    }

    /// The tool-input face of [`override_model`](Self::override_model): folds one
    /// tool's `(model, backend)` override args onto `cast`'s `role` slot. A
    /// backend arg without its model arg is a loud parameter error naming both
    /// spellings — there is no configured id to guess at on a foreign backend.
    fn apply_model_override(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: Option<&str>,
        backend: Option<&str>,
        model_arg: &str,
        backend_arg: &str,
    ) -> Result<(), McpError> {
        match (model, backend) {
            (Some(model), backend) => self.override_model(cast, role, model, backend),
            (None, Some(_)) => Err(McpError::invalid_params(
                format!(
                    "{backend_arg} was sent without {model_arg} — a backend override \
                     needs the model id to run there"
                ),
                None,
            )),
            (None, None) => Ok(()),
        }
    }

    /// Resolve one of `cast`'s slots into a live [`Arm`] for `role`. A missing
    /// slot is the loud call-time gap ("cast `x` has no synth slot" — absent =
    /// capability absent); a backend that fails to build (key resolution,
    /// client init) is an internal error.
    fn arm(&self, cast: &Cast, role: ModelRole) -> Result<Arm, McpError> {
        let slot = cast
            .require_slot(role)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        // The slot's backend ref is canonical (load) or alias-resolved (override),
        // so a failure here is a server bug, not a caller mistake.
        let backend = self
            .config
            .resolve_backend(&slot.backend)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Arm::from_slot(backend, slot, role, &self.config.defaults)
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Resolve a cast's `image` slot into an image generator (a *capability*, not a
    /// model loop). A missing slot is the loud call-time gap (absent = capability
    /// absent, same as `arm`); a non-openai backend is refused inside `from_slot`
    /// (rig 0.38 has no image path for the keyed protocols). Both surface as a
    /// parameter error the caller can act on — pick a cast with an openai `image`
    /// slot, or configure one.
    fn image_gen(&self, cast: &Cast) -> Result<Arc<dyn ImageGen>, McpError> {
        let slot = cast
            .require_slot(ModelRole::Image)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let backend = self
            .config
            .resolve_backend(&slot.backend)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        crate::image_gen::rig_openai(backend, slot)
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))
    }

    #[tool(
        description = "Ask a model *outside your own family* about a codebase and get \
            a grounded, cited answer — kaibo is the door to a second opinion from \
            DeepSeek, Gemini, Anthropic, or a local model. Pick which one answers with \
            `cast` (e.g. `deepseek`, `gemini`, `anthropic`; the `cast` enum lists the \
            teams live right now, `kaibo://config` has the full set). A capable model \
            drives a read-only kaish shell (cat/grep/find/jq/pipelines): it reads \
            precise spans directly and delegates broad repo sweeps to a fast explorer \
            sub-agent, then answers with concrete `file:line` citations. It finds and \
            reads the relevant code itself — describe what you did or want to know \
            (what you changed and why, the behavior in question) in prose; don't paste \
            a diff or dump files, kaibo reads the real, current source from disk and \
            your intent is the part it can't recover on its own. Read-only: it never \
            modifies the project. For a multi-turn conversation, pass a stable \
            session_id: kaibo replays that session's prior question/answer pairs (the \
            exploration still runs fresh each turn). Args: question (required), context \
            (optional; a change summary or pasted source to seed the investigation — \
            trusted as starting evidence), path (project dir; optional if the server \
            has a default root), cast (optional), session_id (optional; opaque \
            conversation key), include_report (optional; attach the explorer's report \
            as structured_content for debugging the hand-off), and optional \
            explorer_model / synth_model overrides (a model id, sent verbatim; add \
            explorer_backend / synth_backend to retarget the slot's backend). For a \
            thin answer with no codebase access, use `oneshot` instead."
    )]
    async fn consult(
        &self,
        Parameters(input): Parameters<ConsultInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        // Resolve the cast, layer per-call model overrides onto the clone, then
        // resolve each phase's slot into its own arm (client + request shape).
        let mut cast = self.resolve_cast(input.cast)?;
        self.apply_model_override(
            &mut cast,
            ModelRole::Explorer,
            input.explorer_model.as_deref(),
            input.explorer_backend.as_deref(),
            "explorer_model",
            "explorer_backend",
        )?;
        self.apply_model_override(
            &mut cast,
            ModelRole::Synth,
            input.synth_model.as_deref(),
            input.synth_backend.as_deref(),
            "synth_model",
            "synth_backend",
        )?;
        let explorer = self.arm(&cast, ModelRole::Explorer)?;
        let synth = self.arm(&cast, ModelRole::Synth)?;
        // Progress rides the whole investigation: sweeps and direct reads emit beats
        // onto the wire when the client supplied a token, else a no-op sink.
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let cfg = ConsultConfig {
            explorer_max_turns: input
                .explorer_max_turns
                .unwrap_or(defaults.explorer_max_turns),
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            sandbox: self.config.sandbox.clone(),
            progress: progress.clone(),
            house_rules: self.house_rules(&root)?,
            prompts: self.resolved_prompts(&cast),
            orientation: self.orientation(&root).await?,
        };

        // Multi-turn: a session_id binds this turn to a thread (replay prior turns,
        // record this one); without one it's a stateless one-shot. The replay/record
        // glue lives in `consult_session_turn` (offline-tested) — the session mutex is
        // only ever touched there, never held across the consult await.
        let session = input.session_id.as_deref().map(|id| (&self.sessions, id));

        // The root span for this tool call's trace: it parents both phases'
        // `run_phase` spans (and through them rig's GenAI tree), so the explore and
        // synth model loops land in ONE trace instead of two orphan roots. Inert
        // unless an exporter is attached.
        let span = tracing::info_span!(
            "consult",
            cast = %cast.name,
            explorer_model = %explorer.model,
            synth_model = %synth.model,
            session = session.is_some(),
        );
        progress.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        let out = match consult(
            &input.question,
            input.context.as_deref(),
            root,
            &explorer,
            &synth,
            &cfg,
            session,
        )
        .instrument(span)
        .await
        {
            Ok(out) => out,
            // A provider/model-loop failure is a clean tool-result error the host can
            // proceed past, not a JSON-RPC internal_error. See `consultation_failed`.
            Err(e) => return Ok(consultation_failed("consult", &cast.name, e)),
        };
        progress.emit(PhaseEvent::PhaseFinished { phase: "consult" });

        // Provenance: name the cast and the models that answered, so a caller (a
        // cross-model study especially) sees which model produced this without
        // digging into `kaibo://config`. consult runs two arms — both are named.
        let answer = with_provenance(
            out.answer,
            &cast.name,
            &[("explorer", &explorer.model), ("synth", &synth.model)],
        );
        Ok(consult_result(answer, out.report, input.include_report))
    }

    #[tool(
        description = "Ask a model *outside your own family* a thin, direct question \
            — prompt in, answer out, no codebase access and no tools. The counterpart \
            to `consult`: use `oneshot` when you already own the context (you've pasted \
            what's needed, or the question is general) and just want another model's \
            take; use `consult` when kaibo should investigate a codebase for you. Pick \
            which model answers with `cast` (e.g. `deepseek`, `gemini`, `anthropic`; \
            the `cast` enum lists the teams live now, `kaibo://config` has the full \
            set) — kaibo runs the cast's capable (synth) model. Exactly one upstream \
            request; you are responsible for any context the model needs. Args: prompt \
            (required), cast (optional), and an optional model (with optional backend) \
            override."
    )]
    async fn oneshot(
        &self,
        Parameters(input): Parameters<OneshotInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let mut cast = self.resolve_cast(input.cast)?;
        self.apply_model_override(
            &mut cast,
            ModelRole::Synth,
            input.model.as_deref(),
            input.backend.as_deref(),
            "model",
            "backend",
        )?;
        let arm = self.arm(&cast, ModelRole::Synth)?;
        // Read + containment-check the attachments (same boundary as a session root); the
        // bytes are inlined server-side so they never transit the calling agent's context.
        let attachments = self.resolve_attachments(&input.attach)?;
        // Gate image attachments on the model's vision capability (shared with batch).
        self.gate_image_attachments(arm.caps.vision, &attachments, &arm.model, &cast.name)?;
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let cfg = ConsultConfig {
            explorer_max_turns: defaults.explorer_max_turns,
            synth_max_turns: defaults.synth_max_turns,
            sandbox: self.config.sandbox.clone(),
            progress: progress.clone(),
            // oneshot reads no project: no house rules, no repo map, no shell.
            house_rules: None,
            prompts: self.resolved_prompts(&cast),
            orientation: None,
        };

        let span = tracing::info_span!("oneshot", cast = %cast.name, model = %arm.model);
        progress.emit(PhaseEvent::PhaseStarted { phase: "oneshot" });
        let answer = match oneshot(&input.prompt, &attachments, &arm, &cfg)
            .instrument(span)
            .await
        {
            Ok(answer) => answer,
            // A provider failure is a clean tool-result error, same as `consult`.
            Err(e) => return Ok(consultation_failed("oneshot", &cast.name, e)),
        };
        progress.emit(PhaseEvent::PhaseFinished { phase: "oneshot" });

        let answer = with_provenance(answer, &cast.name, &[("model", &arm.model)]);
        Ok(CallToolResult::success(vec![Content::text(answer)]))
    }

    #[tool(
        description = "Run a kaish (sh-like) script against the read-only project; \
            returns exit code + stdout + stderr. Browse code with line numbers, and \
            read generously — `cat -n FILE` for a whole file (reach for it first), \
            `grep -rn PATTERN .` to locate across files, `cat -n FILE | sed -n \
            '40,80p'` for a slice of a large one; compose \
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
        // The direct-shell tool gets its own trace (no model loop under it). The kaish
        // worker is `!Send` on its own thread, but this span wraps the async `.await`
        // here, so the script's wall-clock is captured from the caller side — no span
        // crosses the thread boundary.
        let span = tracing::info_span!("run_kaish");
        let out = worker
            .run(input.script)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format_output(
            &out,
        ))]))
    }

    #[tool(
        description = "Generate an image from a text prompt and return it inline. A \
            capability tool: no codebase investigation, no shell — kaibo's image model \
            (the cast's `image` slot) draws what you describe and hands back the picture \
            as an image plus a short caption. The cast must carry an `image` slot on an \
            OpenAI-compatible backend (hosted gpt-image/DALL·E, or a local \
            Stable-Diffusion server speaking /v1/images/generations); see kaibo://config \
            for the configured casts. The image rides back inline (size-capped); a \
            picture too large to inline is a clear error, not a silent drop. Args: \
            prompt (required), cast (optional), size (\"WxH\", default 1024x1024), \
            image_model (override the slot's model id) + image_backend (retarget it, \
            works even on a cast with no image slot)."
    )]
    pub async fn generate_image(
        &self,
        Parameters(input): Parameters<GenerateImageInput>,
    ) -> Result<CallToolResult, McpError> {
        let mut cast = self.resolve_cast(input.cast.clone())?;
        // Optional per-call override on the image slot: `image_model` keeps the slot's
        // backend; `image_backend` retargets it (and works on a cast with no image slot
        // at all). The "pass the backend override arg" error now names a real arg.
        self.apply_model_override(
            &mut cast,
            ModelRole::Image,
            input.image_model.as_deref(),
            input.image_backend.as_deref(),
            "image_model",
            "image_backend",
        )?;
        let generator = self.image_gen(&cast)?;
        let size = crate::generate_image::parse_size(input.size.as_deref())
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // A capability call: a single provider request, no model loop. The span
        // captures its wall-clock from the caller side.
        let span = tracing::info_span!("generate_image", cast = %cast.name, size = ?size);
        // Categorize honestly: a backend/provider failure is ours (internal_error); an
        // unusable result (unrecognized format, over the inline cap) is caller-fixable
        // (invalid_params) — so the agent changes the request rather than giving up.
        use crate::generate_image::GenerateError;
        let image = match crate::generate_image::generate(generator.as_ref(), &input.prompt, size)
            .instrument(span)
            .await
        {
            Ok(image) => image,
            Err(e @ GenerateError::Unusable(_)) => {
                return Err(McpError::invalid_params(e.to_string(), None));
            }
            Err(e @ GenerateError::Backend(_)) => {
                return Err(McpError::internal_error(e.to_string(), None));
            }
        };

        Ok(CallToolResult::success(crate::generate_image::to_content(
            &image,
            &input.prompt,
            size,
        )))
    }

    /// Resolve a cast's synth slot for batch: its slot + backend, plus the model's
    /// resolved [`ModelCaps`]. Cheap and key-free — it does *not* build a network client,
    /// so the caller can resolve attachments and gate on capability (both request-shaping,
    /// not connection) before paying for a provider. A missing synth slot is the loud
    /// call-time gap; the batch-lane check rides the later `batch::submitter` build. The
    /// caps come from the same slot the provider will use, so the gate and the wire agree
    /// on which model runs. Returns the whole `ModelCaps` (not just `vision`) so a future
    /// audio/video attachment gate has its answer without growing this signature.
    fn batch_synth<'a>(
        &'a self,
        cast: &'a Cast,
    ) -> Result<(&'a ModelSlot, &'a Backend, ModelCaps), McpError> {
        let slot = cast
            .require_slot(ModelRole::Synth)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let backend = self
            .config
            .resolve_backend(&slot.backend)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);
        Ok((slot, backend, caps))
    }

    /// A poll/cancel-only provider for a handle's backend. Poll and cancel need only the
    /// connection (key + endpoint), so this re-addresses a batch by id after a restart —
    /// kaibo holds no state.
    fn batch_poller(
        &self,
        backend_name: &str,
    ) -> Result<Arc<dyn crate::batch::BatchProvider>, McpError> {
        let backend = self
            .config
            .resolve_backend(backend_name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        crate::batch::poller(backend).map_err(|e| McpError::invalid_params(format!("{e:#}"), None))
    }

    /// The set of backend names `batch_list` should query. An explicit `backend` scopes
    /// to that one (resolved by name/alias, and refused loudly if its kind has no batch
    /// lane). Omitted, it's every configured batch-capable backend, sorted — the orphan-
    /// recovery default. No batch-capable backend at all is a clear parameter error, not an
    /// empty list pretending nothing's there.
    fn batch_backends(&self, backend: Option<&str>) -> Result<Vec<String>, McpError> {
        if let Some(name) = backend {
            let b = self
                .config
                .resolve_backend(name)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            if !crate::batch::batch_supported(b.kind) {
                return Err(McpError::invalid_params(
                    format!(
                        "backend {:?} ({:?}) has no batch lane, so it can't be listed \
                         (batch-capable: {}). Omit `backend` to list every batch-capable \
                         backend.",
                        b.name,
                        b.kind,
                        crate::batch::supported_kinds_list()
                    ),
                    None,
                ));
            }
            return Ok(vec![b.name.clone()]);
        }
        let names: Vec<String> = self
            .config
            .backends
            .values()
            .filter(|b| crate::batch::batch_supported(b.kind))
            .map(|b| b.name.clone())
            .collect();
        if names.is_empty() {
            return Err(McpError::invalid_params(
                "no batch-capable backend is configured".to_string(),
                None,
            ));
        }
        Ok(names)
    }

    #[tool(
        description = "Submit a batch of tool-less questions to run *offline* at max \
            thinking, and get back a handle to poll — the async sibling of `oneshot`. \
            Use it to fan many prompts (or one hard question you'll wait on) at a \
            top-tier model without holding a call open per answer: the provider runs \
            them on its cheaper batch lane and kaibo hands back the id. Each prompt is \
            self-contained — no codebase access, no tools (batch can't drive a tool \
            loop) — so include the context each answer needs, the same as `oneshot`. \
            Batch maxes the knobs (forces high effort + a generous token budget) \
            regardless of how the cast was tuned for interactive use. Runs the cast's \
            synth model on a backend that supports batch; point `cast`/`model` at one (or \
            get a clear refusal naming the batch-capable backends). `kaibo://config` lists \
            the casts. Args: prompts (required, a list), cast \
            (optional), model + backend (optional synth override — handy to batch a \
            Pro/Opus tier a cast synths cheaper interactively). Returns a handle; poll \
            it with `batch_get`, stop it with `batch_cancel`. This is a fire-and-forget \
            lane: submit, then go do other work — don't sit in a wait/sleep loop holding \
            your turn open. A batch can take minutes to hours (the provider's offline \
            SLA is up to 24h), and the handle is durable — it survives a server restart \
            — so come back and `batch_get` later rather than blocking on it now."
    )]
    pub async fn batch_submit(
        &self,
        Parameters(input): Parameters<BatchSubmitInput>,
    ) -> Result<CallToolResult, McpError> {
        if input.prompts.is_empty() {
            return Err(McpError::invalid_params(
                "batch needs at least one prompt".to_string(),
                None,
            ));
        }
        let mut cast = self.resolve_cast(input.cast)?;
        self.apply_model_override(
            &mut cast,
            ModelRole::Synth,
            input.model.as_deref(),
            input.backend.as_deref(),
            "model",
            "backend",
        )?;
        let (slot, backend, caps) = self.batch_synth(&cast)?;
        let backend_name = backend.name.clone();
        let model = slot.id.clone();
        // Read + containment-check the attachments before anything hits the network: a
        // bad path is a clean refusal, not a half-submitted batch. The bytes are inlined
        // server-side so they never transit the calling agent's context.
        let attachments = self.resolve_attachments(&input.attach)?;
        // Gate image attachments on the synth model's vision capability before the
        // provider is built — so a vision misconfig needs no key to report.
        self.gate_image_attachments(caps.vision, &attachments, &model, &cast.name)?;
        // Now build the network client (resolves the key); a batch-incapable backend is
        // refused honestly here.
        let provider = crate::batch::submitter(backend, slot, &self.config.defaults)
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        let items: Vec<crate::batch::BatchItem> = input
            .prompts
            .iter()
            .enumerate()
            .map(|(i, p)| crate::batch::BatchItem {
                custom_id: i.to_string(),
                prompt: p.clone(),
            })
            .collect();
        // Batch is the oneshot *shape* (a capable model answering from what it was
        // handed, no tools) but its own behavioral contract — one offline response, no
        // follow-up, spend on depth — so it carries a distinct preamble, overridable via
        // `[prompts].batch`. Reads no project (no map / house rules), like oneshot.
        let system = crate::consult::batch_system_prompt(self.config.prompts.batch.as_deref());
        let span =
            tracing::info_span!("batch_submit", cast = %cast.name, model = %model, n = items.len());
        let provider_id = provider
            .submit(&system, &attachments, &items)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        // The handle namespaces the provider id by backend, so poll/cancel re-address it
        // without re-specifying the cast. The split is unambiguous because a *backend
        // name* carries no '/' (enforced at config load) — so the first '/' is always the
        // backend/id boundary, even when the provider id itself contains slashes (a Gemini
        // id is `batches/<id>`).
        let handle = format!("{backend_name}/{provider_id}");
        let msg = format!(
            "Submitted batch `{handle}` — {} prompt(s) on cast `{}` (model `{}`). \
             Poll it with `batch_get` (it'll show progress, then per-item answers when \
             done); stop it with `batch_cancel`.",
            items.len(),
            cast.name,
            model
        );
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Poll a batch by the handle `batch_submit` returned. While it runs \
            you get a progress line; once it's done you get every item's answer (each \
            labelled by its index), with per-item failures surfaced rather than dropped. \
            kaibo holds no state — the handle is the whole address, so this works across \
            a server restart. Poll occasionally, not in a tight loop: if it's still \
            pending, go do other work and check back later rather than sleeping on it — \
            the handle keeps, so there's no rush and nothing to lose. Args: batch_id \
            (the handle from `batch_submit`)."
    )]
    async fn batch_get(
        &self,
        Parameters(input): Parameters<BatchHandleInput>,
    ) -> Result<CallToolResult, McpError> {
        let (backend_name, provider_id) = parse_batch_handle(&input.batch_id)?;
        let provider = self.batch_poller(backend_name)?;
        let span = tracing::info_span!("batch_get", handle = %input.batch_id);
        let poll = provider
            .poll(provider_id)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let label = format!("{backend_name} · {provider_id}");
        Ok(CallToolResult::success(vec![Content::text(
            crate::batch::render_poll(&poll, &label),
        )]))
    }

    #[tool(
        description = "Cancel a running batch by its handle. The provider stops scheduling \
            new requests; any already in flight finish. Poll with `batch_get` afterward \
            for the final per-item results. Args: batch_id (the handle from \
            `batch_submit`)."
    )]
    async fn batch_cancel(
        &self,
        Parameters(input): Parameters<BatchHandleInput>,
    ) -> Result<CallToolResult, McpError> {
        let (backend_name, provider_id) = parse_batch_handle(&input.batch_id)?;
        let provider = self.batch_poller(backend_name)?;
        let span = tracing::info_span!("batch_cancel", handle = %input.batch_id);
        provider
            .cancel(provider_id)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Requested cancellation of batch `{}`. Poll it with `batch_get` for the \
             final per-item results.",
            input.batch_id
        ))]))
    }

    #[tool(
        description = "List the batches a backend still knows about, newest first — the \
            way back to a batch whose handle you've lost (kaibo holds no state, so the \
            provider's own list is the source of truth). Each entry comes with its \
            ready-to-use handle, status, and progress; feed one to `batch_get` or \
            `batch_cancel`. Omit `backend` to list across every batch-capable backend; \
            pass one (name or alias) to scope it. A backend that \
            can't be reached (no key, endpoint down) is reported rather than hiding the \
            rest, and a truncated page says so. Args: backend (optional)."
    )]
    async fn batch_list(
        &self,
        Parameters(input): Parameters<BatchListInput>,
    ) -> Result<CallToolResult, McpError> {
        let backends = self.batch_backends(input.backend.as_deref())?;
        let mut entries: Vec<(String, crate::batch::BatchListItem)> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        let mut truncated: Vec<String> = Vec::new();
        for name in backends {
            // Build the poller and list per backend, turning any failure into a
            // per-backend note — one keyless or unreachable backend never sinks the
            // whole listing (the per-item-failure ethos, at the backend grain).
            let listed = match self.batch_poller(&name) {
                Ok(provider) => {
                    let span = tracing::info_span!("batch_list", backend = %name);
                    provider.list().instrument(span).await
                }
                Err(e) => Err(anyhow::anyhow!("{}", e.message)),
            };
            match listed {
                Ok((items, has_more)) => {
                    if has_more {
                        truncated.push(name.clone());
                    }
                    for it in items {
                        let handle = format!("{}/{}", name, it.provider_id);
                        entries.push((handle, it));
                    }
                }
                Err(e) => errors.push((name, format!("{e:#}"))),
            }
        }
        Ok(CallToolResult::success(vec![Content::text(
            crate::batch::render_list(&entries, &errors, &truncated),
        )]))
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
                // One prompt: `configure`, the guided "set up my models" flow (see
                // `kaibo_prompts`). Advertising `prompts` is what surfaces it in a
                // client's prompt picker / slash menu.
                .enable_prompts()
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
            // Judge provider usability from the live environment so a fresh install
            // (no key, no config) gets setup guidance in the handshake. Read once here,
            // at initialize — the same point the rest of config is bound; reconnecting
            // is what re-reads a newly-set key.
            instructions: Some(kaibo_instructions_with_scope(
                &self.tool_schemas,
                &self.config,
                &self.allowed_set,
                self.default_root.as_deref(),
                self.default_root_inferred,
                self.config
                    .default_cast_usability(|k| std::env::var(k).ok()),
                &self.config.usable_casts(|k| std::env::var(k).ok()),
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
        // Compute the runtime-derived worktree set here (it needs the handler's
        // allowed_set and reflects worktrees that exist *now*); the renderer is a
        // pure function of its inputs, so it can't reach back for this itself.
        read_kaibo_resource_with_config(
            &request.uri,
            &self.tool_schemas,
            &self.config,
            &self.allowed_set,
            self.default_root.as_deref(),
            self.default_root_inferred,
            self.followed_worktrees(),
        )
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            prompts: kaibo_prompts(),
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        kaibo_prompt_messages(&request.name, request.arguments.as_ref())
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
        // The resolved runtime config: allowed paths, default cast, gated tools,
        // sandbox limits, backends (kind + key sources, never key values), and
        // casts with resolved slots. Read this to understand the server's posture.
        RawResource {
            mime_type: Some("application/toml".to_string()),
            description: Some(
                "kaibo's resolved runtime configuration: allowed path trees, default \
                 cast, gated tools, sandbox limits, each backend with its kind and \
                 key sources, and each cast with its resolved slots. Read this to \
                 understand the server's current posture before making calls."
                    .to_string(),
            ),
            ..RawResource::new(CONFIG_URI, "kaibo: runtime config")
        }
        .no_annotation(),
        // The annotated config template — every knob, commented, ready to copy to
        // ~/.config/kaibo/config.toml. The setup guidance on a fresh install points here.
        RawResource {
            mime_type: Some("application/toml".to_string()),
            description: Some(
                "An annotated kaibo config template: every option with its default and a \
                 comment, plus example backends and casts. Copy to \
                 ~/.config/kaibo/config.toml and edit. Pairs with kaibo://config, which \
                 shows the *resolved* runtime state."
                    .to_string(),
            ),
            ..RawResource::new(CONFIG_EXAMPLE_URI, "kaibo: config example")
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

/// The one prompt kaibo advertises: a guided "set up my models" flow.
const CONFIGURE_PROMPT_NAME: &str = "configure";

/// The `configure` prompt body. It hands the calling agent kaibo's *own* config
/// resources and the real config.toml shape (env/file key sources, family-mixing
/// casts) instead of restating the manual, so "configure kaibo" is a grounded flow
/// rather than freehand. Positive framing throughout — name the good idiom, not the
/// prohibition (the house prompt discipline, see AGENTS.md).
const CONFIGURE_PROMPT_BODY: &str = "\
You're configuring **kaibo**, the MCP server you're connected to right now — it lends \
your work a second opinion from models outside your own family. This sets up which \
models it uses.

Work through these steps:

1. Read kaibo's two config resources first (they're MCP resources — no tool turn spent):
   • `kaibo://config/example` — the annotated config.toml template, every knob explained.
   • `kaibo://config` — the resolved live state: the casts and backends that exist now, \
and where each key is sourced from.
2. Ask me which providers I can actually reach before writing anything: which of \
Anthropic / DeepSeek / Gemini I hold API keys for, and whether I run any \
OpenAI-compatible local servers (llama.cpp, Ollama, an image server) and at what base \
URLs. Let me tell you my providers rather than guessing them.
3. Propose a roster built on a provider I actually named in step 2, then write it to \
`$XDG_CONFIG_HOME/kaibo/config.toml` (default `~/.config/kaibo/config.toml`). The \
default shape is a single outside family — DeepSeek, Gemini, Anthropic, or a local pair \
— with explorer and synth both within it. That one family is already the whole win: it \
augments my own lineage with a different house's eyes (a cheap, fast explorer and a \
stronger synth, same family). kaibo's built-in casts are already within-family pairs, \
so often this is just giving one of them a key rather than writing a new cast. Mixing \
families across roles (a 'chimera' — say a DeepSeek explorer with a Claude synth) is an \
advanced move for someone who holds several keys and asks for it; don't reach for it by \
default.
4. Keep secrets in the environment or a key file. A backend names an env var \
(`api_key_env`) or a key-file path (`api_key_file`); the TOML carries the name or path, \
the secret stays outside it. Tell me which env vars to set or files to write, and let \
me put the keys in myself.
5. (Optional) Read scope. By default kaibo reads only the project tree it's pointed at \
(plus linked git worktrees), and it only ever *reads* — never writes. If a workflow \
hands kaibo artifacts to read from a scratch space — a diff, a generated file, a log \
dropped in a temp dir — name that space in `[server] allow_paths` so it's in bounds. \
Prefer the host-portable form `allow_paths = [\"$TMPDIR\"]` or \
`[\"$XDG_RUNTIME_DIR/scratch\"]` over a hardcoded `/tmp` — `$VAR` / `${VAR}` and a \
leading `~` expand in these paths, resolving per machine. Ask me whether I want a scratch \
space readable before adding one: it widens what the team can see (read-only, but still a \
real boundary), so it's a deliberate opt-in, not a default.
6. When the file is written, remind me to reconnect the kaibo MCP server so it re-reads \
the config and keys — both load once at startup.";

/// The prompts kaibo advertises (`list_prompts`). Currently just `configure`.
fn kaibo_prompts() -> Vec<Prompt> {
    vec![Prompt::new(
        CONFIGURE_PROMPT_NAME,
        Some(
            "Guide your agent through writing a kaibo config.toml: it reads kaibo's own \
             config resources, asks which providers and models you have, and writes the \
             file. Pass an optional `goal` to steer the roster.",
        ),
        Some(vec![PromptArgument {
            name: "goal".to_string(),
            title: Some("Setup goal".to_string()),
            description: Some(
                "What you want from the setup, e.g. \"a local-only privacy cast\" or \
                 \"a cheap DeepSeek explorer with a Claude synth\". Optional — omit for a \
                 general walk-through."
                    .to_string(),
            ),
            required: Some(false),
        }]),
    )]
}

/// Resolve a prompt name + arguments into its messages (`get_prompt`). Pure — no peer
/// or IO — so the prompt content is unit-testable; the trait method is a thin wrapper.
/// An unknown name is a loud `invalid_params`, never a silent empty prompt.
fn kaibo_prompt_messages(
    name: &str,
    arguments: Option<&JsonObject>,
) -> Result<GetPromptResult, McpError> {
    match name {
        CONFIGURE_PROMPT_NAME => {
            // A blank/whitespace goal reads as "no goal" — don't append an empty line.
            let goal = arguments
                .and_then(|a| a.get("goal"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Ok(GetPromptResult {
                description: Some("Configure kaibo's models for this codebase".to_string()),
                messages: vec![PromptMessage::new_text(
                    PromptMessageRole::User,
                    configure_prompt_text(goal),
                )],
            })
        }
        other => Err(McpError::invalid_params(
            format!("unknown prompt {other:?}; kaibo offers: {CONFIGURE_PROMPT_NAME}"),
            None,
        )),
    }
}

/// The `configure` body, with an optional caller `goal` appended verbatim.
fn configure_prompt_text(goal: Option<&str>) -> String {
    let mut body = String::from(CONFIGURE_PROMPT_BODY);
    if let Some(goal) = goal {
        body.push_str("\n\nMy goal for this setup: ");
        body.push_str(goal);
    }
    body
}

/// The URI templates kaibo advertises: per-builtin help, addressed by name.
fn kaibo_resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
    let template = RawResourceTemplate {
        uri_template: BUILTIN_URI_TEMPLATE.to_string(),
        name: "kaish builtin help".to_string(),
        title: None,
        description: Some(
            "Help for a single kaish builtin — parameters and examples. \
             e.g. kaibo://kaish/builtin/grep"
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
/// allowed trees, default cast, gated tools, sandbox limits, tunable defaults,
/// each backend's kind/endpoint/key sources, and each cast's slots as
/// `"backend/id"` with *resolved* caps — so a calling model or operator sees the
/// server's current posture at a glance.
///
/// SECRET-SAFETY CONTRACT: this function renders key SOURCE metadata (env var names,
/// key file paths — the operator-configured pointers) but NEVER the resolved key
/// values. The backend struct stores sources, not secrets; this renderer reads only
/// those source fields. If Config ever gains a resolved-key cache, do not read it here.
/// Tests in this file assert the contract holds.
fn render_config_resource(
    config: &Config,
    allowed_set: &[PathBuf],
    default_root: Option<&Path>,
    default_root_inferred: bool,
    followed_worktrees: Vec<PathBuf>,
) -> String {
    use serde::Serialize;
    use std::collections::BTreeMap;

    // Dedicated render-only shapes — plain Serialize structs that carry exactly what
    // the resource must expose and nothing more. Keeps the contract explicit.

    #[derive(Serialize)]
    struct ConfigDoc {
        /// Allowed path trees: a per-call path must be at-or-under one of these.
        allowed_paths: Vec<String>,
        /// The effective default root a call uses when it omits `path` — an explicit
        /// `--root`, or the launch cwd kaibo inferred. Absent when neither applies.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_root: Option<String>,
        /// True when `default_root` was inferred from the launch cwd rather than
        /// configured explicitly. Only meaningful when `default_root` is present.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        default_root_inferred: bool,
        /// Default cast name (what a call omitting `cast` gets).
        default_cast: String,
        /// Runtime-derived state — computed at read time, not configured. Distinct
        /// from the static knobs above so a reader can tell "what kaibo discovered"
        /// from "what the operator set".
        runtime: RuntimeDoc,
        /// Which tools are currently advertised.
        tools: ToolsDoc,
        /// Read-only sandbox limits.
        sandbox: SandboxDoc,
        /// kaish kernel behavior tuning (the `[kaish]` stanza) — currently the
        /// resolved ignore policy the file-walking builtins honor.
        kaish: KaishDoc,
        /// The [defaults] tunables every slot falls back to.
        defaults: DefaultsDoc,
        /// OpenTelemetry export state (off by default). Header *names* only — a
        /// value could be a bearer token, so it's withheld like an API key.
        telemetry: TelemetryDoc,
        /// alias → canonical backend name. Aliases are valid slot-ref prefixes
        /// and per-call backend overrides, so callers must be able to discover
        /// them here — built-in and file-declared both.
        backend_aliases: BTreeMap<String, String>,
        /// Backends (connections): kind, endpoint, key sources (never key values).
        backends: BTreeMap<String, BackendDoc>,
        /// alias → canonical cast name (each a valid `cast` call-param value).
        cast_aliases: BTreeMap<String, String>,
        /// Casts (compositions): slots as "backend/id" with resolved caps.
        casts: BTreeMap<String, CastDoc>,
    }

    #[derive(Serialize)]
    struct ToolsDoc {
        consult: bool,
        oneshot: bool,
        run_kaish: bool,
        generate_image: bool,
        batch: bool,
    }

    /// Runtime-computed scope state. `follow_worktrees` echoes the knob;
    /// `followed_worktrees` is the live extra set the follow feature grants beyond
    /// `allowed_paths` right now — git worktrees of an already-allowed repo,
    /// resolved by reading git's link files. Recomputed on each read, so a worktree
    /// added mid-session shows up here without a reconnect.
    #[derive(Serialize)]
    struct RuntimeDoc {
        follow_worktrees: bool,
        followed_worktrees: Vec<String>,
    }

    #[derive(Serialize)]
    struct SandboxDoc {
        exec_timeout_secs: u64,
        output_limit_bytes: usize,
        /// Cap on the `/` scratch MemoryFs in bytes; a write past it fails loudly.
        scratch_limit_bytes: u64,
        /// Builtins shadow-blocked beyond the structural read-only guards.
        disable_builtins: Vec<String>,
    }

    #[derive(Serialize)]
    struct KaishDoc {
        ignore: IgnoreDoc,
    }

    /// The resolved `[kaish.ignore]` policy the file-walking builtins honor.
    #[derive(Serialize)]
    struct IgnoreDoc {
        /// Ignore filenames loaded (root + ancestors), in precedence order.
        files: Vec<String>,
        /// Built-in defaults (`target/`, `node_modules/`, `.git`) applied.
        defaults: bool,
        /// Nested `.gitignore` files auto-loaded during the walk.
        auto_gitignore: bool,
        /// User's global gitignore (`core.excludesFile`) honored.
        global_gitignore: bool,
        /// `"enforced"` (all walkers incl. `find`) or `"advisory"` (polite tools only).
        scope: &'static str,
    }

    #[derive(Serialize)]
    struct DefaultsDoc {
        explorer_max_turns: usize,
        synth_max_turns: usize,
        max_tokens: u64,
        thinking_budget: u64,
        explorer_temperature: f64,
        synth_temperature: f64,
        top_p: f64,
        explorer_effort: String,
        synth_effort: String,
        thinking_style: String,
        request_timeout_secs: u64,
        session_capacity: usize,
    }

    /// Telemetry as resolved. SECRET-SAFETY: `header_names` lists the keys of any
    /// configured export headers but never their values — an Authorization value is
    /// a secret, same as an API key. The operator set the names; surfacing those is
    /// the discoverability the resource promises.
    #[derive(Serialize)]
    struct TelemetryDoc {
        enabled: bool,
        endpoint: String,
        timeout_secs: u64,
        service_name: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        header_names: Vec<String>,
    }

    #[derive(Serialize)]
    struct BackendDoc {
        kind: String,
        /// Resolved endpoint for openai-kind backends (explicit base_url, else
        /// OPENAI_BASE_URL, else the built-in default) — the "resolved runtime
        /// state" promise. Other kinds have fixed endpoints baked into rig.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
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
        request_timeout_secs: u64,
    }

    /// One cast slot: the `"backend/id"` ref plus its *resolved* capabilities
    /// (slot pin applied, else the classifier on the slot's backend kind) and any
    /// per-slot tunable overrides actually set — the effective runtime state.
    #[derive(Serialize)]
    struct SlotDoc {
        model: String,
        vision: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_budget: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_style: Option<String>,
        /// The per-model system-prompt override, verbatim (not a secret — it's the
        /// operator's own framing). Absent when unset.
        #[serde(skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
        /// Per-slot tunables that *are* set here but this slot's resolved request shape
        /// will never send — the honest no-op flag. A `thinking_budget` on an
        /// effort-driven or toggle-less model, an `effort` on a budget model, a
        /// `temperature` an Anthropic slot drops under thinking: each load-validates and
        /// would otherwise render as if effective. Absent when every set knob has a sink.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        inert_tunables: Vec<&'static str>,
    }

    /// A cast's role table, keyed by role. Only configured roles appear.
    type CastDoc = BTreeMap<&'static str, SlotDoc>;

    let backends: BTreeMap<String, BackendDoc> = config
        .backends
        .iter()
        .map(|(name, b)| {
            // Exhaustive destructure — any new Backend field is a compile error
            // here, forcing an explicit render-or-skip decision (including the
            // secret-safety review for any field that might resolve a key value).
            let crate::config::Backend {
                name: _,
                kind,
                base_url,
                api_key_env,
                api_key_file,
                key_optional,
                request_timeout,
            } = b;
            let rendered_base_url = if *kind == crate::credentials::ProviderKind::Openai {
                Some(b.resolved_base_url())
            } else {
                base_url.clone()
            };
            let doc = BackendDoc {
                kind: format!("{:?}", kind).to_lowercase(),
                base_url: rendered_base_url,
                // KEY SOURCE ONLY — env var name or file path, never the value.
                api_key_env: api_key_env.clone(),
                api_key_file: api_key_file.clone(),
                key_optional: *key_optional,
                request_timeout_secs: request_timeout.as_secs(),
            };
            (name.clone(), doc)
        })
        .collect();

    let casts: BTreeMap<String, CastDoc> = config
        .casts
        .iter()
        .map(|(name, cast)| {
            let slots: CastDoc = cast
                .slots
                .iter()
                .map(|(role, slot)| {
                    // Exhaustive destructure, same discipline as Backend above.
                    let ModelSlot {
                        backend: _,
                        id: _,
                        vision: _,
                        max_tokens,
                        thinking_budget,
                        temperature,
                        effort,
                        thinking_style,
                        preamble,
                    } = slot;
                    let caps = config
                        .slot_caps(slot)
                        .expect("a loaded cast's slot backend resolves");
                    // Resolve the slot's request shape so we can flag tunables it will
                    // never send (e.g. a budget on an effort-driven model) — making the
                    // invisible no-op visible rather than rendering it as if effective.
                    let kind = config
                        .resolve_backend(&slot.backend)
                        .expect("a loaded cast's slot backend resolves")
                        .kind;
                    let shape =
                        ModelShape::resolve(kind, &slot.id, thinking_style.unwrap_or_default());
                    let mut inert_tunables = Vec::new();
                    if thinking_budget.is_some() && !shape.sinks_thinking_budget() {
                        inert_tunables.push("thinking_budget");
                    }
                    if effort.is_some() && !shape.sinks_effort() {
                        inert_tunables.push("effort");
                    }
                    if temperature.is_some() && !shape.sinks_sampling() {
                        inert_tunables.push("temperature");
                    }
                    (
                        role.key(),
                        SlotDoc {
                            model: slot.qualified(),
                            vision: caps.vision,
                            max_tokens: *max_tokens,
                            thinking_budget: *thinking_budget,
                            temperature: *temperature,
                            effort: effort.clone(),
                            thinking_style: thinking_style.map(|s| format!("{s:?}").to_lowercase()),
                            preamble: preamble.clone(),
                            inert_tunables,
                        },
                    )
                })
                .collect();
            (name.clone(), slots)
        })
        .collect();

    // Exhaustive destructures, same discipline as Backend/ModelSlot above: a new
    // field on any of these is a compile error here, forcing an explicit
    // render-or-skip decision instead of silently vanishing from the resource.
    let &ToolGating {
        consult,
        oneshot,
        run_kaish,
        generate_image,
        batch,
    } = &config.tools;
    let crate::sandbox::SandboxConfig {
        exec_timeout,
        output_limit_bytes,
        scratch_limit_bytes,
        disable_builtins,
        ignore,
    } = &config.sandbox;
    let crate::config::Defaults {
        explorer_max_turns,
        synth_max_turns,
        max_tokens,
        thinking_budget,
        explorer_temperature,
        synth_temperature,
        top_p,
        explorer_effort,
        synth_effort,
        thinking_style,
        request_timeout,
        session_capacity,
    } = &config.defaults;
    let crate::config::TelemetryConfig {
        enabled: telemetry_enabled,
        endpoint: telemetry_endpoint,
        headers: telemetry_headers,
        timeout: telemetry_timeout,
        service_name: telemetry_service_name,
    } = &config.telemetry;
    let doc = ConfigDoc {
        allowed_paths: allowed_set
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        default_root: default_root.map(|p| p.display().to_string()),
        default_root_inferred,
        default_cast: config.default_cast.clone(),
        runtime: RuntimeDoc {
            follow_worktrees: config.follow_worktrees,
            followed_worktrees: followed_worktrees
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        },
        tools: ToolsDoc {
            consult,
            oneshot,
            run_kaish,
            generate_image,
            batch,
        },
        sandbox: SandboxDoc {
            exec_timeout_secs: exec_timeout.as_secs(),
            output_limit_bytes: *output_limit_bytes,
            scratch_limit_bytes: *scratch_limit_bytes,
            disable_builtins: disable_builtins.clone(),
        },
        kaish: KaishDoc {
            ignore: IgnoreDoc {
                files: ignore.files().to_vec(),
                defaults: ignore.use_defaults(),
                auto_gitignore: ignore.auto_gitignore(),
                global_gitignore: ignore.use_global_gitignore(),
                scope: match ignore.scope() {
                    kaish_kernel::IgnoreScope::Enforced => "enforced",
                    kaish_kernel::IgnoreScope::Advisory => "advisory",
                },
            },
        },
        defaults: DefaultsDoc {
            explorer_max_turns: *explorer_max_turns,
            synth_max_turns: *synth_max_turns,
            max_tokens: *max_tokens,
            thinking_budget: *thinking_budget,
            explorer_temperature: *explorer_temperature,
            synth_temperature: *synth_temperature,
            top_p: *top_p,
            explorer_effort: explorer_effort.clone(),
            synth_effort: synth_effort.clone(),
            thinking_style: format!("{thinking_style:?}").to_lowercase(),
            request_timeout_secs: request_timeout.as_secs(),
            session_capacity: session_capacity.get(),
        },
        telemetry: TelemetryDoc {
            enabled: *telemetry_enabled,
            endpoint: telemetry_endpoint.clone(),
            timeout_secs: telemetry_timeout.as_secs(),
            service_name: telemetry_service_name.clone(),
            header_names: telemetry_headers.keys().cloned().collect(),
        },
        backend_aliases: config.backend_aliases().clone(),
        backends,
        cast_aliases: config.cast_aliases().clone(),
        casts,
    };

    // Serialize to TOML. If the TOML serializer rejects something (unlikely given
    // all fields are primitive strings/ints/bools), crash loudly rather than return
    // a silently truncated or misleading document — the caller would get a half-truth.
    let body = toml::to_string_pretty(&doc).expect(
        "config render structs are TOML-serializable; if this panics, a field type changed",
    );
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
    default_root: Option<&Path>,
    default_root_inferred: bool,
    followed_worktrees: Vec<PathBuf>,
) -> Result<ReadResourceResult, McpError> {
    if uri == CONFIG_URI {
        let body = render_config_resource(
            config,
            allowed_set,
            default_root,
            default_root_inferred,
            followed_worktrees,
        );
        return Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(body, uri)],
        });
    }
    if uri == CONFIG_EXAMPLE_URI {
        // Static, config-independent — the embedded template verbatim.
        return Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(CONFIG_EXAMPLE_TOML, uri)],
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

/// How a runtime consultation failure should be framed to the calling agent — derived
/// from the error chain by [`classify_failure`].
#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    /// A transient provider condition (overload / rate-limit / timeout / reset). Worth a
    /// caller-driven manual retry.
    TransientProvider,
    /// A non-transient model/provider error (auth, bad request). Retrying won't help.
    Provider,
    /// A kaibo-*side* failure (e.g. the synth's kaish kernel failed to build) — not the
    /// provider's fault, so we must not say it was.
    Internal,
}

/// Classify a consultation failure from its error chain. This is a **heuristic on the
/// error text**, by necessity: rig collapses the HTTP status into the response *body*
/// (`CompletionError::ProviderError(text)` carries Anthropic's `overloaded_error` JSON, a
/// Gemini `RESOURCE_EXHAUSTED`, etc. — not the number `529`), so we match the providers'
/// transient *vocabulary* rather than a status code. The model loop wraps its errors as
/// `"model loop failed: …"` (`consult.rs`); an error chain lacking that marker came from
/// *before* a model ran (a kaish kernel build inside the toolset factory), so it's a
/// kaibo-side failure, not the provider's.
fn classify_failure(err: &anyhow::Error) -> FailureKind {
    let s = format!("{err:#}").to_lowercase();
    let from_model_loop = s.contains("model loop failed") || s.contains("model used all");
    if !from_model_loop {
        return FailureKind::Internal;
    }
    // Transient vocabulary across Anthropic / Gemini / OpenAI / DeepSeek bodies and the
    // transport layer (reqwest timeouts/resets from our own `request_timeout`).
    const TRANSIENT: &[&str] = &[
        "overload",        // Anthropic 529 overloaded_error, Gemini
        "rate limit",      // generic
        "rate_limit",      // OpenAI/DeepSeek/Anthropic error `type`s
        "ratelimit",
        "resource_exhausted", // Gemini 429
        "too many requests",  // 429 reason phrase
        "timed out",          // reqwest / gateway
        "timeout",
        "connection reset",
        "reset by peer",
        "connection closed",
        "broken pipe",
        "temporarily",     // "temporarily unavailable"
        "unavailable",     // 503 / Gemini UNAVAILABLE
        "try again",
    ];
    if TRANSIENT.iter().any(|t| s.contains(t)) {
        FailureKind::TransientProvider
    } else {
        FailureKind::Provider
    }
}

/// Surface a *runtime* consultation failure as a **tool-result error** (`is_error =
/// true`) rather than a protocol-level `internal_error`. A consult is an *optional*
/// augmentation: the calling agent should read a clear message and proceed *without* the
/// second opinion — not have its own tool call fail at the JSON-RPC layer. The framing is
/// tailored by [`classify_failure`] so the agent can drive the right next step: a
/// transient overload/timeout invites a manual retry (kaibo does **not** retry on its own
/// — one completion is bounded by the backend's `request_timeout`/`connect_timeout`; see
/// the failure-policy FAQ and `docs/config.md`), a non-transient provider error doesn't,
/// and a kaibo-side failure is named honestly rather than blamed on the provider. Setup
/// errors *before* the model call — unknown cast, an attachment outside the boundary, a
/// missing key — stay `McpError`, since those are the caller's to fix.
fn consultation_failed(tool: &str, cast: &str, err: anyhow::Error) -> CallToolResult {
    let detail = format!("{err:#}");
    let guidance = match classify_failure(&err) {
        FailureKind::TransientProvider => {
            "This looks like a transient provider condition (overload, rate limit, or \
             timeout). kaibo does not retry automatically — you may retry this call, or \
             proceed without the consultation."
        }
        FailureKind::Provider => {
            "The model or its provider rejected the request; retrying is unlikely to help \
             — proceed without the consultation, or check the cast and config."
        }
        FailureKind::Internal => {
            "This is a kaibo-side error (not the provider) — please report it; you can \
             still proceed without the consultation."
        }
    };
    CallToolResult::error(vec![Content::text(format!(
        "{tool} could not complete (cast `{cast}`): {detail}. {guidance}"
    ))])
}

/// Append a one-line provenance footer naming the cast and the model(s) that
/// produced `answer`. The point is legibility: a caller — a cross-model study most
/// of all — should see *which* model answered without cross-referencing
/// `kaibo://config`, since the answering model is the whole variable. `roles` is the
/// labelled models for this tool (one for `oneshot`, explorer+synth for `consult`).
/// Pure and offline-testable.
/// Split a batch handle (`"backend/provider-id"`) the way `batch_submit` minted it.
/// Splitting on the *first* `/` is unambiguous because a backend name carries no `/`
/// (enforced at config load) — so the provider id keeps any slashes of its own (an
/// Anthropic id is `msgbatch_…`; a Gemini id is `batches/<id>`). A malformed handle is a
/// loud parameter error — the caller pasted something that wasn't a kaibo batch id.
fn parse_batch_handle(handle: &str) -> Result<(&str, &str), McpError> {
    handle
        .split_once('/')
        .filter(|(b, id)| !b.is_empty() && !id.is_empty())
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "batch id {handle:?} must be \"backend/provider-id\" — pass the handle \
                     kaibo returned from batch_submit"
                ),
                None,
            )
        })
}

fn with_provenance(answer: String, cast: &str, roles: &[(&str, &str)]) -> String {
    let models = roles
        .iter()
        .map(|(label, model)| format!("{label} `{model}`"))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{answer}\n\n———\nkaibo · cast `{cast}` · {models}")
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
        f.debug_struct("ProgressReporter")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl ProgressReporter {
    fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        Self {
            peer,
            token,
            seq: Arc::new(AtomicU64::new(0)),
        }
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
    use rmcp::model::{NumberOrString, PromptMessageContent};
    use rmcp::ServerHandler;

    /// A small stand-in builtin set so resource rendering is offline-testable.
    fn sample_schemas() -> Vec<ToolSchema> {
        vec![
            ToolSchema::new("cat", "Read a file"),
            ToolSchema::new("grep", "Search files for a pattern"),
        ]
    }

    fn handler() -> KaiboHandler {
        KaiboHandler::new(Config::builtin()).expect("handler builds")
    }

    fn handler_from_toml(toml: &str) -> KaiboHandler {
        KaiboHandler::new(Config::from_toml_str(toml).expect("config parses"))
            .expect("handler builds")
    }

    /// The live cast roster is stamped onto each consultation tool's `cast` param as
    /// a JSON-Schema `enum`, so an agent reads the menu off the schema it fills
    /// arguments from — the fix for casts being discoverable only in handshake prose
    /// a host may truncate. The keyless local `openai` cast is always usable, so it
    /// anchors the assertion regardless of which API keys the test env carries.
    #[test]
    fn consultation_tools_advertise_the_live_cast_enum() {
        let h = handler();
        for tool in ["consult", "oneshot"] {
            let schema = h
                .tool_router
                .get(tool)
                .expect("tool advertised")
                .input_schema
                .clone();
            let variants = schema
                .get("properties")
                .and_then(|p| p.get("cast"))
                .and_then(|c| c.get("enum"))
                .and_then(|e| e.as_array())
                .unwrap_or_else(|| panic!("{tool}: cast param should carry an enum:\n{schema:#?}"));
            assert!(
                variants.iter().any(|v| v == "openai"),
                "{tool}: cast enum should list the always-usable local cast, got {variants:?}"
            );
        }
    }

    /// `generate_image` advertises its own, *differently-filtered* roster: casts with a
    /// usable `image` slot on an openai backend — not the explorer/synth `usable_casts`.
    /// So a config-only cast that carries an openai `image` slot shows up, while a cast
    /// with no image slot (or one on a non-openai backend) does not.
    #[test]
    fn generate_image_advertises_image_capable_casts_only() {
        let h = handler_from_toml(
            r#"
            # An openai `image` slot on the keyless local backend → image-capable.
            [casts.art]
            image = { backend = "openai", id = "sd-xl" }

            # An `image` slot on a non-openai backend → rig has no path → excluded.
            [casts.wrongkind]
            image = { backend = "anthropic", id = "imagen-ish" }
            "#,
        );
        let schema = h
            .tool_router
            .get("generate_image")
            .expect("generate_image advertised")
            .input_schema
            .clone();
        let variants: Vec<&str> = schema
            .get("properties")
            .and_then(|p| p.get("cast"))
            .and_then(|c| c.get("enum"))
            .and_then(|e| e.as_array())
            .unwrap_or_else(|| panic!("generate_image cast param should carry an enum:\n{schema:#?}"))
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            variants.contains(&"art"),
            "the openai image cast should be listed, got {variants:?}"
        );
        assert!(
            !variants.contains(&"wrongkind"),
            "a non-openai image cast has no rig path and must not be listed: {variants:?}"
        );
        assert!(
            !variants.contains(&"openai"),
            "the builtin `openai` cast has no image slot and must not be listed: {variants:?}"
        );
    }

    /// An empty roster (no cast can reach a model) leaves `cast` enum-free: an empty
    /// `enum` would read as "no valid value" and wrongly forbid the optional field.
    /// `inject_cast_enum` is the seam — driving it with `[]` keeps the test honest
    /// without fabricating a keyless-everything config.
    #[test]
    fn empty_cast_roster_leaves_the_param_unconstrained() {
        let mut router = KaiboHandler::tool_router();
        inject_cast_enum(&mut router, &["consult", "oneshot"], &[]);
        let schema = router
            .get("consult")
            .expect("tool present")
            .input_schema
            .clone();
        assert!(
            schema
                .get("properties")
                .and_then(|p| p.get("cast"))
                .and_then(|c| c.get("enum"))
                .is_none(),
            "an empty roster must not stamp an enum:\n{schema:#?}"
        );
    }

    /// A per-model slot `preamble` wins over the global `[prompts].<phase>`, and the
    /// synth slot feeds *both* capable-model phases (the `consult` driver and the
    /// toolless `oneshot`) — the "its own, even if a copy" shape: same value today,
    /// but each arrives under its own key, free to diverge.
    #[test]
    fn slot_preamble_wins_over_phase_prompts_and_feeds_both_synth_phases() {
        let h = handler_from_toml(
            r#"
            [prompts]
            explorer = "EXP_PHASE"
            oneshot = "ONE_PHASE"
            consult = "CON_PHASE"

            [casts.team]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", preamble = "EXP_SLOT" }
            synth = { backend = "anthropic", id = "claude-opus-4-8", preamble = "SYNTH_SLOT" }
            "#,
        );
        let cast = h.resolve_cast(Some("team".into())).unwrap();
        let p = h.resolved_prompts(&cast);
        // Slot wins over the phase prompt for the explorer...
        assert_eq!(p.explorer.as_deref(), Some("EXP_SLOT"));
        // ...and the synth slot's voice reaches BOTH capable-model phases, each via
        // its own key (a copy for now, independently addressable).
        assert_eq!(p.consult.as_deref(), Some("SYNTH_SLOT"));
        assert_eq!(p.oneshot.as_deref(), Some("SYNTH_SLOT"));
    }

    /// With no slot preambles, the global `[prompts]` is the fallback — and the two
    /// capable-model phases keep *independent* keys, so the toolless `oneshot` can
    /// differ from the `consult` driver.
    #[test]
    fn phase_prompts_are_the_fallback_and_synth_phases_stay_independent() {
        let h = handler_from_toml(
            r#"
            [prompts]
            oneshot = "ONESHOT_ONLY"
            consult = "DRIVER_ONLY"

            [casts.team]
            explorer = "anthropic/claude-haiku-4-5"
            synth = "anthropic/claude-opus-4-8"
            "#,
        );
        let cast = h.resolve_cast(Some("team".into())).unwrap();
        let p = h.resolved_prompts(&cast);
        assert!(p.explorer.is_none(), "no explorer prompt set anywhere");
        // The two capable-model phases diverge — proof they're not collapsed into one.
        assert_eq!(p.oneshot.as_deref(), Some("ONESHOT_ONLY"));
        assert_eq!(p.consult.as_deref(), Some("DRIVER_ONLY"));
    }

    /// A per-call model override (a bare slot) carries no preamble — so overriding
    /// the model doesn't silently drag along the configured slot's framing.
    #[test]
    fn a_per_call_model_override_carries_no_slot_preamble() {
        let h = handler_from_toml(
            r#"
            [casts.team]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", preamble = "EXP_SLOT" }
            synth = "anthropic/claude-opus-4-8"
            "#,
        );
        let mut cast = h.resolve_cast(Some("team".into())).unwrap();
        // Simulate a per-call explorer model override → bare slot, preamble dropped.
        h.apply_model_override(
            &mut cast,
            ModelRole::Explorer,
            Some("claude-haiku-4-5"),
            None,
            "model",
            "backend",
        )
        .unwrap();
        let p = h.resolved_prompts(&cast);
        assert!(
            p.explorer.is_none(),
            "a bare (per-call-override) slot must carry no preamble, got {:?}",
            p.explorer
        );
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
        let event = PhaseEvent::SweepStarted {
            question: "where is X?".into(),
        };
        let p = progress_param(token.clone(), 3, &event);
        assert_eq!(p.progress_token, token);
        assert_eq!(p.progress, 3.0);
        assert!(
            p.total.is_none(),
            "a consult's step count isn't known up front"
        );
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
            templates
                .iter()
                .any(|t| t.raw.uri_template == BUILTIN_URI_TEMPLATE),
            "must advertise the per-builtin URI template"
        );
    }

    /// The handshake must advertise the `prompts` capability, else a client never
    /// asks for the `configure` prompt — the menu entry would be invisible.
    #[test]
    fn handshake_advertises_prompts_capability() {
        let info = handler().get_info();
        assert!(
            info.capabilities.prompts.is_some(),
            "prompts capability must be enabled so clients surface the configure prompt"
        );
    }

    /// `list_prompts` offers exactly the `configure` prompt, with its optional `goal`
    /// argument declared not-required (a required arg would make the bare prompt fail).
    #[test]
    fn lists_the_configure_prompt() {
        let prompts = kaibo_prompts();
        let configure = prompts
            .iter()
            .find(|p| p.name == CONFIGURE_PROMPT_NAME)
            .expect("configure prompt must be advertised");
        let goal = configure
            .arguments
            .as_ref()
            .and_then(|args| args.iter().find(|a| a.name == "goal"))
            .expect("configure must declare a `goal` argument");
        assert_eq!(
            goal.required,
            Some(false),
            "`goal` must be optional so the bare prompt works"
        );
    }

    /// The prompt body is the whole point: it must route the agent to kaibo's own
    /// config resources and the real config.toml shape, not restate the manual. If a
    /// resource URI or the secret-handling contract drifts, this fails.
    #[test]
    fn configure_prompt_grounds_in_the_config_resources() {
        let result =
            kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, None).expect("configure must resolve");
        let PromptMessageContent::Text { text } = &result.messages[0].content else {
            panic!("configure prompt must be a text message");
        };
        for needle in [
            CONFIG_EXAMPLE_URI, // read the annotated template
            CONFIG_URI,         // and the resolved live state
            "config.toml",      // write target
            "api_key_env",      // keys-by-reference, not inline
            "reconnect",        // re-read at startup
        ] {
            assert!(
                text.contains(needle),
                "configure prompt should mention {needle:?}; body:\n{text}"
            );
        }
    }

    /// The default roster is a within-family explorer/synth pair — one outside family
    /// already augments the calling agent. Cross-family mixing (a chimera) is demoted to
    /// an advanced, opt-in move, not the path the agent walks by default. Pins both so
    /// the steer can't drift back to pushing a chimera.
    #[test]
    fn configure_prompt_defaults_to_a_within_family_pair_not_a_chimera() {
        let result =
            kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, None).expect("configure must resolve");
        let PromptMessageContent::Text { text } = &result.messages[0].content else {
            panic!("configure prompt must be a text message");
        };
        assert!(
            text.contains("both within it"),
            "the default must be explorer and synth within one family; body:\n{text}"
        );
        assert!(
            text.contains("advanced move"),
            "a chimera must be framed as an advanced move; body:\n{text}"
        );
        assert!(
            text.contains("don't reach for it by default"),
            "the prompt must tell the agent not to default to a chimera; body:\n{text}"
        );
    }

    /// A supplied `goal` is woven into the message so the agent tailors the roster;
    /// a blank one is treated as absent (no dangling "goal:" line).
    #[test]
    fn configure_prompt_weaves_in_a_goal() {
        let args = json!({ "goal": "a local-only privacy cast" });
        let with_goal = kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, args.as_object())
            .expect("configure must resolve");
        let PromptMessageContent::Text { text } = &with_goal.messages[0].content else {
            panic!("expected text");
        };
        assert!(
            text.contains("a local-only privacy cast"),
            "a supplied goal must appear in the prompt; body:\n{text}"
        );

        let blank = json!({ "goal": "   " });
        let without = kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, blank.as_object())
            .expect("configure must resolve");
        let PromptMessageContent::Text { text } = &without.messages[0].content else {
            panic!("expected text");
        };
        assert!(
            !text.contains("My goal for this setup:"),
            "a blank goal must not append an empty goal line; body:\n{text}"
        );
    }

    /// An unknown prompt name is a loud `invalid_params`, never a silent empty prompt
    /// the agent would run blind.
    #[test]
    fn unknown_prompt_is_a_loud_error() {
        let err = kaibo_prompt_messages("does-not-exist", None)
            .expect_err("an unknown prompt name must error");
        assert!(
            err.message.contains("does-not-exist") && err.message.contains(CONFIGURE_PROMPT_NAME),
            "error should name the bad prompt and the real one, got: {}",
            err.message
        );
    }

    fn read_text(uri: &str, schemas: &[ToolSchema]) -> String {
        // Use the config-aware dispatch for all URIs — same path the handler takes.
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        let result =
            read_kaibo_resource_with_config(uri, schemas, &config, &allowed, None, false, vec![])
                .expect("known uri must read");
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    #[test]
    fn reads_the_sandbox_doc_with_the_idioms_and_codes() {
        let text = read_text(SANDBOX_URI, &[]);
        for needle in ["cat -n", "grep", "read-only", "126", "124"] {
            assert!(text.contains(needle), "sandbox doc must mention {needle:?}");
        }
    }

    #[test]
    fn reads_a_topic_resource() {
        let text = read_text(&format!("{KAISH_RES_PREFIX}syntax"), &[]);
        assert!(
            text.contains("Variables"),
            "syntax topic should cover Variables:\n{text}"
        );
    }

    #[test]
    fn reads_a_builtin_resource_and_rejects_an_unknown_builtin() {
        let schemas = sample_schemas();
        let text = read_text(&format!("{BUILTIN_PREFIX}grep"), &schemas);
        assert!(
            text.contains("grep"),
            "builtin help should name the tool:\n{text}"
        );
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        assert!(
            read_kaibo_resource_with_config(
                &format!("{BUILTIN_PREFIX}nope"),
                &schemas,
                &config,
                &allowed,
                None,
                false,
                vec![],
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
            read_kaibo_resource_with_config(
                "kaibo://nope",
                &[],
                &config,
                &allowed,
                None,
                false,
                vec![]
            )
            .is_err(),
            "an unknown URI must be a not-found error, not an empty success"
        );
    }

    /// The text channel of a result (the answer). Panics if it isn't a single
    /// text block, which is the only shape `consult_result` produces.
    fn answer_text(result: &CallToolResult) -> String {
        assert_eq!(
            result.content.len(),
            1,
            "consult result is a single text block"
        );
        result.content[0]
            .as_text()
            .expect("consult answer is text content")
            .text
            .clone()
    }

    /// A runtime consultation failure surfaces as a **tool-result error** (`is_error =
    /// true`) carrying the detail — not a protocol-level `internal_error` — so the calling
    /// agent reads "the consult failed, here's why" and proceeds without the second
    /// opinion. The message names the tool and cast and preserves the underlying chain.
    #[test]
    fn consultation_failed_is_a_tool_error_carrying_the_detail() {
        let err = anyhow::anyhow!("model loop failed: ProviderError: overloaded_error");
        let result = consultation_failed("consult", "deepseek", err);
        assert_eq!(
            result.is_error,
            Some(true),
            "a provider failure is a tool-result error, not a success"
        );
        let text = answer_text(&result);
        assert!(text.contains("consult"), "names the tool: {text}");
        assert!(text.contains("deepseek"), "names the cast: {text}");
        assert!(
            text.contains("overloaded_error"),
            "preserves the underlying detail so the host can decide: {text}"
        );
    }

    /// A *transient* provider condition (overload / rate-limit / timeout / reset) is
    /// classified as retryable, so the message invites the calling agent to drive a manual
    /// retry. We match the providers' transient *vocabulary*, not a status number: rig
    /// collapses the HTTP status into the response *body* (`ProviderError(text)`), so the
    /// numeric code isn't reliably present.
    #[test]
    fn transient_provider_failure_suggests_a_manual_retry() {
        for body in [
            "model loop failed: ProviderError: {\"type\":\"overloaded_error\"}",
            "model loop failed: ProviderError: rate_limit_error",
            "model loop failed: HttpError: error sending request: operation timed out",
            "model loop failed: HttpError: connection reset by peer",
            "model loop failed: ProviderError: RESOURCE_EXHAUSTED",
        ] {
            let result = consultation_failed("consult", "gemini", anyhow::anyhow!(body));
            let text = answer_text(&result).to_lowercase();
            assert!(
                text.contains("retry"),
                "a transient failure should invite a manual retry: {body} -> {text}"
            );
        }
    }

    /// A *non-transient* provider error (auth / bad request) does not invite a retry —
    /// retrying won't help — but is still a clean tool-result error.
    #[test]
    fn non_transient_provider_failure_does_not_suggest_retry() {
        let err = anyhow::anyhow!("model loop failed: ProviderError: invalid_request_error");
        let text = answer_text(&consultation_failed("consult", "anthropic", err));
        assert!(
            !text.to_lowercase().contains("you may retry")
                && !text.to_lowercase().contains("retry this call"),
            "a non-transient error must not invite a retry: {text}"
        );
    }

    /// A kaibo-*side* failure (a kaish kernel build, not the model loop) must not be
    /// blamed on the provider — the message names it as a kaibo internal error. (DeepSeek
    /// review, 2026-06-23: the synth's kernel spawns inside the consult error shadow, so a
    /// spawn failure would otherwise read as "the provider failed, proceed without it".)
    #[test]
    fn internal_failure_is_not_blamed_on_the_provider() {
        let err = anyhow::anyhow!("failed to build read-only kaish kernel: out of memory");
        let text = answer_text(&consultation_failed("consult", "deepseek", err));
        let lower = text.to_lowercase();
        assert!(
            lower.contains("kaibo"),
            "a kaibo-side failure is named as such, not the provider's fault: {text}"
        );
        assert!(
            !lower.contains("provider failed") && !lower.contains("provider rejected"),
            "must not claim the provider failed: {text}"
        );
    }

    /// Provenance footer: the answer keeps its text, and the cast plus every labelled
    /// model is appended so a caller (a cross-model study most of all) sees which model
    /// produced the answer without cross-referencing `kaibo://config`.
    #[test]
    fn provenance_footer_names_the_cast_and_every_model() {
        let out = with_provenance(
            "the answer".into(),
            "gemini",
            &[
                ("explorer", "gemini-flash-lite-latest"),
                ("synth", "gemini-3.5-flash"),
            ],
        );
        assert!(
            out.starts_with("the answer"),
            "the answer is preserved: {out}"
        );
        assert!(out.contains("cast `gemini`"), "names the cast: {out}");
        assert!(
            out.contains("explorer `gemini-flash-lite-latest`"),
            "names the explorer model: {out}"
        );
        assert!(
            out.contains("synth `gemini-3.5-flash`"),
            "names the synth model: {out}"
        );

        // The oneshot shape: a single labelled model.
        let one = with_provenance("x".into(), "deepseek", &[("model", "deepseek-v4-pro")]);
        assert!(
            one.contains("cast `deepseek` · model `deepseek-v4-pro`"),
            "{one}"
        );
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
        assert_eq!(
            sc["report"], "src/x.rs:1 the snippet",
            "report rides under `report`"
        );
    }

    /// Opt-in with an empty report (the consult delegated no sweep): still surfaced.
    /// Emptiness is the signal — present-but-empty means "asked, no sweep happened",
    /// which a caller must be able to tell apart from "never asked" (None).
    #[test]
    fn consult_result_surfaces_empty_report_when_requested() {
        let result = consult_result("ans".into(), String::new(), true);
        let sc = result
            .structured_content
            .expect("requested even when empty");
        assert_eq!(sc["report"], "", "an empty report is surfaced honestly");
    }

    #[test]
    fn instructions_compose_the_canonical_onboarding_and_point_at_resources() {
        use crate::kaish_syntax::kaibo_instructions;
        let text = kaibo_instructions(&sample_schemas());
        assert!(
            text.contains("kaibo"),
            "instructions should introduce kaibo"
        );
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

    // --- kaibo://config/example resource tests -------------------------------

    /// The embedded config example is listed and readable, and — the drift guard —
    /// it must still parse as a valid `Config`. The day someone changes a config
    /// field and forgets the example, this fails instead of shipping a template that
    /// errors when a fresh user copies it.
    #[test]
    fn config_example_resource_is_listed_readable_and_valid() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.raw.uri).collect();
        assert!(
            uris.iter().any(|u| u == CONFIG_EXAMPLE_URI),
            "kaibo_resources() must list kaibo://config/example, got {uris:?}"
        );

        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp")];
        let result = read_kaibo_resource_with_config(
            CONFIG_EXAMPLE_URI,
            &[],
            &config,
            &allowed,
            None,
            false,
            vec![],
        )
        .expect("example resource must be readable");
        let body = match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text contents, got {other:?}"),
        };
        // It's the real template (a recognizable anchor), and it parses — so a fresh
        // user who copies it verbatim gets a working config, not a load error.
        assert!(
            body.contains("[backends.anthropic]"),
            "example must be the annotated template:\n{body}"
        );
        crate::config::Config::from_toml_str(&body)
            .expect("the embedded config example must parse as a valid Config");
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

    /// The `[runtime]` section surfaces the live follow state: the knob, plus the
    /// worktrees admitted *beyond* the static allowed set right now (passed in by
    /// the handler, which computes them at read time). This keeps `kaibo://config`
    /// honest about the real boundary — an auto-followed sibling isn't in
    /// `allowed_paths` but is reachable, and a reader must be able to see that.
    #[test]
    fn config_resource_runtime_section_reports_followed_worktrees() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/the-repo")];
        let followed = vec![std::path::PathBuf::from("/tmp/the-repo-feature")];
        let body = render_config_resource(&config, &allowed, None, false, followed);
        assert!(
            body.contains("[runtime]") && body.contains("follow_worktrees = true"),
            "runtime section must echo the follow knob:\n{body}"
        );
        assert!(
            body.contains("/tmp/the-repo-feature"),
            "runtime section must list the followed worktree:\n{body}"
        );
    }

    /// A per-slot tunable that the slot's resolved request shape will never send is
    /// flagged `inert_tunables` in the render, so the operator sees the no-op instead of
    /// a knob that looks effective. The matrix: a budget on an effort-driven model
    /// (Gemini 3-line, Anthropic adaptive) or the toggle-less openai path; an effort on
    /// a budget model; a temperature an Anthropic slot drops under thinking. A knob that
    /// *does* have a sink is never flagged.
    #[test]
    fn config_render_flags_inert_per_slot_tunables() {
        let config = Config::from_toml_str(
            r#"
            # Gemini 3-line: takes thinkingLevel (effort), no budget.
            [casts.gem]
            explorer = { backend = "gemini", id = "gemini-3-pro", thinking_budget = 4096, effort = "low" }

            # openai (toggle-less): sends neither effort nor budget; keeps sampling.
            [casts.oai]
            synth = { backend = "openai", id = "gemma-local", thinking_budget = 8192, effort = "high", temperature = 0.7 }

            # Anthropic budget tier: takes budget_tokens, no effort; drops sampling under thinking.
            [casts.ant_budget]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", effort = "high", temperature = 0.5 }

            # Anthropic adaptive: takes output_config.effort, no budget.
            [casts.ant_adaptive]
            synth = { backend = "anthropic", id = "claude-opus-4-8", effort = "high", thinking_budget = 2048 }
            "#,
        )
        .unwrap();
        let body = render_config_resource(&config, &[], None, false, vec![]);
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let inert = |cast: &str, role: &str| -> Vec<String> {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .map(|a| {
                    a.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            inert("gem", "explorer"),
            vec!["thinking_budget"],
            "Gemini 3-line sinks effort (thinkingLevel) but not a budget"
        );
        assert_eq!(
            inert("oai", "synth"),
            vec!["thinking_budget", "effort"],
            "openai sends neither thinking knob; temperature it does send"
        );
        assert_eq!(
            inert("ant_budget", "explorer"),
            vec!["effort", "temperature"],
            "budget tier ignores effort; Anthropic drops sampling under thinking"
        );
        assert_eq!(
            inert("ant_adaptive", "synth"),
            vec!["thinking_budget"],
            "adaptive sinks effort but rejects a budget"
        );
    }

    /// The config resource body must contain the key structural fields a calling
    /// model or operator expects: allowed paths, default_cast, gated tools,
    /// sandbox limits, backends with kind and key sources, and casts with their
    /// slots rendered as "backend/id" carrying resolved caps.
    #[test]
    fn config_resource_renders_expected_fields() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/test-allowed")];
        let body = render_config_resource(&config, &allowed, None, false, vec![]);
        // Structural presence checks — the resource is TOML or a document, not prose.
        for needle in [
            "allowed_paths",
            "default_cast",
            "[runtime]",
            "follow_worktrees",
            "tools",
            "sandbox",
            "defaults",
            "backends",
            "casts",
        ] {
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
        // Backends and casts include the built-in four.
        for name in ["anthropic", "deepseek", "gemini", "openai"] {
            assert!(
                body.contains(&format!("[backends.{name}]")),
                "config resource must list the {name} backend:\n{body}"
            );
            assert!(
                body.contains(&format!("casts.{name}")),
                "config resource must list the {name} cast:\n{body}"
            );
        }
        // Slots render as "backend/id" with their RESOLVED caps (the classifier on
        // the slot's backend kind: Anthropic sees, DeepSeek is blind).
        assert!(
            body.contains("anthropic/claude-sonnet-4-6"),
            "slots render as backend/id:\n{body}"
        );
        let anthropic_synth = body
            .find("anthropic/claude-sonnet-4-6")
            .map(|i| &body[i..i + 120])
            .unwrap();
        assert!(
            anthropic_synth.contains("vision = true"),
            "anthropic slot carries resolved vision=true:\n{anthropic_synth}"
        );
        let deepseek_synth = body
            .find("deepseek/deepseek-v4-pro")
            .map(|i| &body[i..i + 120])
            .unwrap();
        assert!(
            deepseek_synth.contains("vision = false"),
            "deepseek slot carries resolved vision=false:\n{deepseek_synth}"
        );
        // Key SOURCES (env var name / file path) must appear — operators configured
        // them and need to see them for diagnostics.
        assert!(
            body.contains("ANTHROPIC_API_KEY"),
            "config resource must show key source env var names:\n{body}"
        );
        // Telemetry state is part of the resolved runtime: an operator must be able
        // to see whether kaibo is shipping spans off-box and to where.
        assert!(
            body.contains("[telemetry]") && body.contains("enabled = false"),
            "config resource must show telemetry state (off by default):\n{body}"
        );
    }

    /// SECRET-SAFETY teeth: an export header *value* (e.g. a bearer token) must
    /// never reach the rendered resource — only the header *name*, the pointer the
    /// operator set, exactly as key sources render their env var name not the key.
    #[test]
    fn config_resource_withholds_telemetry_header_values() {
        let config = Config::from_toml_str(
            r#"
            [telemetry]
            enabled = true
            headers = { authorization = "Bearer super-secret-token" }
            "#,
        )
        .unwrap();
        let body = render_config_resource(&config, &[], None, false, vec![]);
        // The header NAME is discoverable…
        assert!(
            body.contains("authorization"),
            "header name must be visible for diagnostics:\n{body}"
        );
        // …but its VALUE is a secret and must not leak.
        assert!(
            !body.contains("super-secret-token") && !body.contains("Bearer"),
            "a header value must never render — it can be a bearer token:\n{body}"
        );
    }

    /// The alias registries are part of the resolved runtime state: an alias is a
    /// valid `cast` value and slot-ref prefix, so a caller reading `kaibo://config`
    /// must be able to discover them — built-ins and file-declared both.
    #[test]
    fn config_resource_renders_backend_and_cast_aliases() {
        let config = Config::from_toml_str(
            r#"
            [backends.big]
            kind = "openai"
            base_url = "http://localhost:9001/v1"
            aliases = ["heavy"]

            [casts.team]
            aliases = ["fast"]
            synth = "heavy/qwen3-235b"
            "#,
        )
        .unwrap();
        let body = render_config_resource(&config, &[], None, false, vec![]);
        for needle in ["[backend_aliases]", "[cast_aliases]"] {
            assert!(body.contains(needle), "must render {needle}:\n{body}");
        }
        // Built-in aliases at both levels, and the file-declared ones.
        for needle in [
            r#"claude = "anthropic""#,
            r#"google = "gemini""#,
            r#"heavy = "big""#,
            r#"fast = "team""#,
        ] {
            assert!(body.contains(needle), "must render {needle}:\n{body}");
        }
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
        let toml = format!("[backends.anthropic]\napi_key_env = \"{var_name}\"\n");
        let config = Config::from_toml_str(&toml).expect("valid config");

        // Set the sentinel in env and render inside the lock.
        let body = {
            let _guard = ENV_LOCK.lock().unwrap();
            // SAFETY: holding the lock means no other test in this binary mutates env.
            #[allow(deprecated)]
            unsafe {
                std::env::set_var(var_name, SENTINEL);
            }
            let b = render_config_resource(&config, &allowed, None, false, vec![]);
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
        let toml2 = format!("[backends.anthropic]\napi_key_file = \"{file_path}\"\n");
        let config2 = Config::from_toml_str(&toml2).expect("valid config");
        let body2 = render_config_resource(&config2, &allowed, None, false, vec![]);
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
        let body_str = render_config_resource(&config, &allowed, None, false, vec![]);
        // Sanity: the rendered document has something in it.
        assert!(
            !body_str.is_empty(),
            "config resource body must not be empty"
        );
        // The dispatch must not return not-found for this URI.
        let result = read_kaibo_resource_with_config(
            CONFIG_URI,
            &[],
            &config,
            &allowed,
            None,
            false,
            vec![],
        );
        assert!(
            result.is_ok(),
            "kaibo://config must be readable, got {result:?}"
        );
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
        let text = kaibo_instructions_with_scope(
            &schemas,
            &config,
            &allowed,
            None,
            false,
            crate::config::CastUsability::Ready,
            &[],
        );
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

    /// When there is an explicit default root, the scope section must name it and
    /// must NOT tag it as inferred.
    #[test]
    fn instructions_scope_section_names_default_root() {
        let schemas = sample_schemas();
        let config = Config::builtin();
        let root = std::path::PathBuf::from("/projects/myapp");
        let allowed = vec![root.clone()];
        let text = kaibo_instructions_with_scope(
            &schemas,
            &config,
            &allowed,
            Some(&root),
            false,
            crate::config::CastUsability::Ready,
            &[],
        );
        assert!(
            text.contains("/projects/myapp"),
            "scope section must name the configured root:\n{text}"
        );
        assert!(
            !text.contains("inferred"),
            "an explicit root must not be tagged inferred:\n{text}"
        );
    }

    /// An inferred default root (from the launch cwd) must be named *and* tagged so
    /// the caller can tell it wasn't configured by hand.
    #[test]
    fn instructions_scope_section_tags_inferred_default_root() {
        let schemas = sample_schemas();
        let config = Config::builtin();
        let root = std::path::PathBuf::from("/work/space");
        let allowed = vec![root.clone()];
        let text = kaibo_instructions_with_scope(
            &schemas,
            &config,
            &allowed,
            Some(&root),
            true,
            crate::config::CastUsability::Ready,
            &[],
        );
        assert!(
            text.contains("/work/space"),
            "scope section must name the inferred root:\n{text}"
        );
        assert!(
            text.to_lowercase().contains("inferred"),
            "an inferred root must be tagged so the boundary stays legible:\n{text}"
        );
    }

    /// When no default root applies the scope section must be honest about it.
    #[test]
    fn instructions_scope_section_states_no_default_root_when_absent() {
        let schemas = sample_schemas();
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp")];
        let text = kaibo_instructions_with_scope(
            &schemas,
            &config,
            &allowed,
            None,
            false,
            crate::config::CastUsability::Ready,
            &[],
        );
        // Must explain that every call must pass a path.
        assert!(
            text.to_lowercase().contains("every call") || text.contains("no default"),
            "scope section must note the absence of a default root:\n{text}"
        );
    }

    // --- The cast param --------------------------------------------------------

    /// `cast` is the param's name and a stale `provider` is now a tombstone: with
    /// the transitional alias removed it falls under `deny_unknown_fields`, so an
    /// old client sending it gets a loud invalid-params error, never a silent drop
    /// into the default cast. (The rmcp-seam coverage lives in tests/cast_param.rs.)
    #[test]
    fn cast_is_the_param_and_a_stale_provider_is_rejected() {
        let input: ConsultInput =
            serde_json::from_value(json!({ "question": "q", "cast": "deepseek" })).unwrap();
        assert_eq!(input.cast.as_deref(), Some("deepseek"));
        let err = serde_json::from_value::<ConsultInput>(
            json!({ "question": "q", "provider": "gemini" }),
        )
        .expect_err("a stale `provider` arg must be a loud unknown-field error");
        assert!(
            err.to_string().contains("provider"),
            "the error must name the unknown field, got: {err}"
        );
    }

    // --- Per-call model overrides over a cast -----------------------------------

    /// A bare-id override swaps the id within the slot: the backend is kept, the
    /// caps pin and per-slot tunables are dropped (the new id classifies fresh).
    #[test]
    fn a_bare_override_keeps_the_backend_and_drops_the_pins() {
        let config = Config::from_toml_str(
            r#"
            [casts.pinned]
            synth = { backend = "openai", id = "llava", vision = true, max_tokens = 999 }
            "#,
        )
        .unwrap();
        let h = KaiboHandler::new(config).unwrap();
        let mut cast = h.resolve_cast(Some("pinned".into())).unwrap();
        h.override_model(&mut cast, ModelRole::Synth, "other-model", None)
            .unwrap();
        let slot = cast.slot(ModelRole::Synth).unwrap();
        assert_eq!(slot.backend, "openai", "backend kept");
        assert_eq!(slot.id, "other-model");
        assert_eq!(slot.vision, None, "caps pin dropped — classifies fresh");
        assert_eq!(slot.max_tokens, None, "per-slot tunables dropped");
    }

    /// The explicit backend arg retargets the slot's backend (aliases resolve),
    /// enabling a call-time chimera.
    #[test]
    fn a_backend_arg_retargets_the_slot() {
        let h = handler();
        let mut cast = h.resolve_cast(Some("anthropic".into())).unwrap();
        h.override_model(
            &mut cast,
            ModelRole::Explorer,
            "deepseek-v4-flash",
            Some("deepseek"),
        )
        .unwrap();
        let slot = cast.slot(ModelRole::Explorer).unwrap();
        assert_eq!(slot.backend, "deepseek");
        assert_eq!(slot.id, "deepseek-v4-flash");
        // Aliases resolve to the canonical backend.
        h.override_model(
            &mut cast,
            ModelRole::Synth,
            "claude-opus-4-8",
            Some("claude"),
        )
        .unwrap();
        assert_eq!(cast.slot(ModelRole::Synth).unwrap().backend, "anthropic");
        // An unknown backend is a loud parameter error naming the known set.
        let err = h
            .override_model(&mut cast, ModelRole::Synth, "some-model", Some("nope"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown backend"), "got: {err}");
    }

    /// A model id containing `/` is still just a model id: a HuggingFace-style
    /// org prefix ("google/…") must ride verbatim to the slot's configured
    /// backend, never be reinterpreted as a backend ref — "google" is a gemini
    /// alias, and silently retargeting the call there is the bug class the house
    /// rules name. Retargeting is the explicit backend arg's job.
    #[test]
    fn an_org_prefixed_model_id_stays_on_the_slots_backend() {
        let h = handler();
        let mut cast = h.resolve_cast(Some("openai".into())).unwrap();
        h.override_model(
            &mut cast,
            ModelRole::Explorer,
            "google/gemma-3-27b-it",
            None,
        )
        .unwrap();
        let slot = cast.slot(ModelRole::Explorer).unwrap();
        assert_eq!(slot.backend, "openai", "the configured backend is kept");
        assert_eq!(slot.id, "google/gemma-3-27b-it", "the id rides verbatim");
    }

    /// An empty or whitespace model override is a typo, never an intent — the
    /// same loud rule config load applies to slots (it would otherwise surface
    /// as a baffling provider 404 mid-call).
    #[test]
    fn an_empty_model_override_is_a_loud_parameter_error() {
        let h = handler();
        let mut cast = h.resolve_cast(Some("anthropic".into())).unwrap();
        for value in ["", "   "] {
            let err = h
                .override_model(&mut cast, ModelRole::Synth, value, None)
                .expect_err("an empty model id must be rejected");
            assert!(err.to_string().contains("model id is empty"), "got: {err}");
        }
    }

    /// A backend override without its model id has nothing to address there —
    /// loud error, not a guess at the configured id on a foreign backend.
    #[test]
    fn a_backend_override_without_a_model_is_a_loud_parameter_error() {
        let h = handler();
        let mut cast = h.resolve_cast(Some("anthropic".into())).unwrap();
        let err = h
            .apply_model_override(
                &mut cast,
                ModelRole::Synth,
                None,
                Some("deepseek"),
                "synth_model",
                "synth_backend",
            )
            .expect_err("backend without model must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("synth_backend"), "names the arg, got: {msg}");
        assert!(msg.contains("synth_model"), "names the fix, got: {msg}");
    }

    /// A bare override on a role the cast doesn't carry can't keep a backend that
    /// isn't there — loud error naming the gap and the backend-arg escape hatch.
    #[test]
    fn a_bare_override_on_a_missing_slot_is_a_loud_error() {
        let config = Config::from_toml_str(
            r#"
            [casts.synthless]
            explorer = "deepseek/deepseek-v4-flash"
            "#,
        )
        .unwrap();
        let h = KaiboHandler::new(config).unwrap();
        let mut cast = h.resolve_cast(Some("synthless".into())).unwrap();
        let err = h
            .override_model(&mut cast, ModelRole::Synth, "bare-id", None)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("has no synth slot"), "got: {msg}");
        assert!(
            msg.contains("backend"),
            "names the escape hatch, got: {msg}"
        );
        // With a backend arg the override works even on the missing slot.
        h.override_model(
            &mut cast,
            ModelRole::Synth,
            "claude-sonnet-4-6",
            Some("anthropic"),
        )
        .unwrap();
        assert!(cast.slot(ModelRole::Synth).is_some());
    }

    /// A cast missing the role a tool needs fails loudly at call time, naming
    /// the gap — absent = capability absent.
    #[test]
    fn arming_a_missing_slot_names_the_gap() {
        let config = Config::from_toml_str(
            r#"
            [casts.synthless]
            explorer = "deepseek/deepseek-v4-flash"
            "#,
        )
        .unwrap();
        let h = KaiboHandler::new(config).unwrap();
        let cast = h.resolve_cast(Some("synthless".into())).unwrap();
        let err = h.arm(&cast, ModelRole::Synth).unwrap_err();
        assert!(err.to_string().contains("has no synth slot"), "got: {err}");
    }

    /// An unknown cast name is a parameter error naming the known casts.
    #[test]
    fn an_unknown_cast_is_a_parameter_error_naming_the_known_casts() {
        let h = handler();
        let err = h.resolve_cast(Some("nope".into())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown cast"), "got: {msg}");
        assert!(msg.contains("anthropic"), "got: {msg}");
    }
}
