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
use tracing::Instrument;

use crate::config::{Backend, Cast, Config, Lane, ModelRole, ModelSlot};
use crate::consult::{
    consult, explore_with, oneshot, Arm, ConsultConfig, ExploreConfig, ModelCaps, PhaseContext,
    PromptOverrides,
};
use crate::explorer::format_output;
use crate::jobs::{CancelOutcome, JobResult, JobState, JobStore};
use crate::kaish_syntax::{
    kaibo_instructions_with_scope, kaibo_sandbox_doc, render_builtin_help, render_topic, topics,
};
use crate::mcp_log;
use crate::progress::{NullSink, PhaseEvent, ProgressLog, ProgressSink, TracingSink};
use crate::sandbox::{builtin_schemas, KaishWorker};
use crate::session::SessionStore;

mod config_resource;
mod containment;
mod render;

use config_resource::render_config_resource;
use render::{
    batch_poll_brief, batch_within_window, consult_result, consultation_failed,
    consultation_failure_text, is_batch_handle, now_epoch_secs, parse_batch_handle, render_job,
    render_jobs_section, render_wait, wait_level_floor, wait_level_label, with_provenance,
    BATCH_RECENCY_WINDOW_SECS,
};

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
/// Long-form "how to wield the tools well" guidance — attachments, cast/model
/// selection, the sync↔async pairs and their handles, and the read-only shell's
/// idioms. The tool schemas stay terse and point here, so the repetition and positive
/// framing that helps a calling model use the tools lives in a resource the host loads
/// on demand, not in every agent's startup context (the AGENTS.md prompt-writing split:
/// terse where it's always loaded, generous where it's pulled).
const TOOLS_URI: &str = "kaibo://tools";
/// The system preambles kaibo hands each model-driven phase — explorer, consult
/// driver, oneshot, and the offline batch/deliberate synth — rendered through the exact
/// same [`resolve_phase_preamble`](crate::consult::resolve_phase_preamble) seam the live
/// tools use (with any active `[prompts]` override folded in), plus the dynamic user-turn
/// framing. Read this to see, verbatim, what a call actually says to the model.
const PROMPTS_URI: &str = "kaibo://prompts";
/// Per-cast prompts: `kaibo://prompts/<cast>` renders that cast's *resolved* framing —
/// its per-slot `preamble`s folded in the way a live call resolves them — so an operator
/// sees exactly what one cast's models are told, not just the cast-independent base.
const PROMPTS_CAST_PREFIX: &str = "kaibo://prompts/";
/// The URI template advertised for the per-cast prompts resource.
const PROMPTS_CAST_URI_TEMPLATE: &str = "kaibo://prompts/{cast}";
/// `docs/config.example.toml`, embedded at compile time so it ships *inside* the
/// binary — `cargo install kaibo` lays down no docs, so reading the file at runtime
/// would 404 at exactly the fresh-install moment the example matters most.
const CONFIG_EXAMPLE_TOML: &str = include_str!("../../docs/config.example.toml");

/// Slack added above a `deliberate`-direct job's synth `request_timeout` when sizing
/// its wall-clock backstop: the per-request reqwest deadline should fire first (a
/// cleaner error), leaving this tokio timer as the true backstop. Small on purpose —
/// deliberate-direct is one completion, so `request_timeout` already sizes the wait.
const DELIBERATE_DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_secs(60);

/// The wall-clock backstop for a `deliberate`-direct job: its synth backend's own
/// `request_timeout` plus [`DELIBERATE_DEADLINE_MARGIN`]. Sized to the *single*
/// completion the direct lane runs — not the interactive-loop `call_deadline` — so a
/// slow local model keeps its full configured patience without forcing the interactive
/// ceiling high (a 3h local `deliberate` needs a 3h `request_timeout` set anyway, and
/// inherits it here). Pure, so the sizing decision is pinned without spawning a job.
fn deliberate_direct_deadline(synth_backend: &Backend) -> std::time::Duration {
    synth_backend.request_timeout + DELIBERATE_DEADLINE_MARGIN
}

/// Which tools to advertise. All on by default; each `--no-<tool>` flips one off.
///
/// Composes to any posture: `{oneshot:false}` ≈ the codebase-only surface; only
/// `run_kaish` on ≈ "no code leaves the box, kaibo as a pure read-only shell". A
/// server with *all* off is a misconfiguration — refused at startup (see `main`),
/// not represented as a valid state here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolGating {
    pub consult: bool,
    /// The single-phase `explore` sweep — its own gate, independent of `consult`
    /// (which carries its own explorer inside the driver loop).
    pub explore: bool,
    /// The `deliberate` tool (explore → offline synth) — its own gate. The offline
    /// deliberation rides the batch (or job) collect verbs, but the tool that *starts*
    /// one is gated here, independent of `consult`/`batch`.
    pub deliberate: bool,
    pub oneshot: bool,
    pub run_kaish: bool,
    /// The batch capability (submit/get/cancel/list) — one gate over all the verbs:
    /// they're one capability (you can't get or list without submit), so `--no-batch`
    /// drops them together rather than a flag apiece.
    pub batch: bool,
}

impl Default for ToolGating {
    fn default() -> Self {
        Self {
            consult: true,
            explore: true,
            deliberate: true,
            oneshot: true,
            run_kaish: true,
            batch: true,
        }
    }
}

impl ToolGating {
    /// True iff every tool is disabled — the zero-tool server we refuse to start.
    pub fn all_disabled(&self) -> bool {
        !self.consult
            && !self.explore
            && !self.deliberate
            && !self.oneshot
            && !self.run_kaish
            && !self.batch
    }
}

/// Arguments to the `consult` tool. `deny_unknown_fields` (here and on every tool
/// input): a typo'd or misplaced argument must be a loud invalid-params error —
/// serde would otherwise drop it and the call would run on configured defaults
/// while the caller believes the override applied. Serde aliases stay accepted.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsultInput {
    /// The question to investigate. Say in prose what you did or want to know — kaibo
    /// locates and reads the real, current code itself, so your intent beats a pasted diff.
    pub question: String,

    /// Optional starting evidence — a change/diff *summary* or pasted source kaibo can't
    /// reach. Trusted: kaibo extends it rather than re-deriving cited spans. Prefer a prose
    /// summary of intent over a raw diff.
    #[serde(default)]
    pub context: Option<String>,

    /// Absolute path to the project to explore. Optional when the server has a default
    /// root; must be at-or-under an allowed tree (`kaibo://config` shows the set).
    #[serde(default)]
    pub path: Option<String>,

    /// Which cast (model team) runs this call; omit for the server's default. Pick from
    /// this param's `enum`; `kaibo://config` lists every cast and backend.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the explorer (investigation) model id. See `kaibo://tools` for override
    /// semantics (ids are verbatim; pair with `explorer_backend` to also retarget).
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Run the explorer override on this backend (name or alias). Requires
    /// `explorer_model`. See `kaibo://tools`.
    #[serde(default)]
    pub explorer_backend: Option<String>,

    /// Override the synthesizer (final-answer) model id. See `kaibo://tools` for override
    /// semantics (pair with `synth_backend` to also retarget).
    #[serde(default)]
    pub synth_model: Option<String>,

    /// Run the synth override on this backend (name or alias). Requires `synth_model`.
    /// See `kaibo://tools`.
    #[serde(default)]
    pub synth_backend: Option<String>,

    /// Opaque session id for a multi-turn consult: kaibo replays this session's prior
    /// `(question, answer)` pairs and records this turn; exploration still runs fresh.
    /// Omit for a stateless call. Sessions are evicted by capacity, not time.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Max tool-loop turns for each delegated `explore′` sweep (default 100).
    #[serde(default)]
    pub explorer_max_turns: Option<usize>,

    /// Max tool-loop turns for the consult driver loop itself (default 200).
    #[serde(default)]
    pub synth_max_turns: Option<usize>,

    /// Attach the explorer's aggregated report as `structured_content` alongside the
    /// answer, for debugging the hand-off. Off by default (it can be large; an empty
    /// report means the consult delegated no sweep).
    #[serde(default)]
    pub include_report: bool,

    /// Workspace files (under the project root) to put in front of the investigation.
    /// Text files are INLINED whole (numbered, up to the inline budget; larger ones the
    /// model is directed to read whole through its shell), images open via `view_image`
    /// (so an attached image needs a vision-capable cast) — and every delegated explorer
    /// sweep is directed to read them too. Hand it the files a question centers on, or
    /// the whole files a change touched. See `kaibo://tools`.
    #[serde(default)]
    pub attach: Vec<String>,
}

/// Arguments to the `explore` tool: a single-phase explorer sweep. Explorer-only —
/// no synth, session, or context (explore reads the repo itself and returns the
/// cited report, not a synthesized answer). `attach` becomes a directive to read
/// each named file whole during the sweep.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExploreInput {
    /// What to survey or map. Say in prose what you want charted — kaibo's explorer
    /// locates and reads the real, current code itself and reports back with citations.
    pub question: String,

    /// Workspace files (under the project root) central to the survey: the investigator
    /// is directed to read each one WHOLE as part of its sweep. Text only — it reads
    /// through the shell, so attach images to `consult` with a vision cast instead.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Absolute path to the project to explore. Optional when the server has a default
    /// root; must be at-or-under an allowed tree (`kaibo://config` shows the set).
    #[serde(default)]
    pub path: Option<String>,

    /// Which cast (model team) runs this call; omit for the server's default. Pick from
    /// this param's `enum`; `kaibo://config` lists every cast and backend.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the explorer (investigation) model id. See `kaibo://tools` for override
    /// semantics (ids are verbatim; pair with `explorer_backend` to also retarget).
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Run the explorer override on this backend (name or alias). Requires
    /// `explorer_model`. See `kaibo://tools`.
    #[serde(default)]
    pub explorer_backend: Option<String>,

    /// Max tool-loop turns for the explorer sweep (default 100).
    #[serde(default)]
    pub explorer_max_turns: Option<usize>,
}

/// Arguments to the `deliberate` tool: `explore → offline synth`. The explorer runs
/// live to build a cited dossier (you wait for this — minutes), then the offline synth
/// deliberates over it. No `session_id`/`context`: deliberate reads the repo itself,
/// and the synth is a single offline turn (so no `synth_max_turns`). `attach` reaches
/// the dossier-building explorer as read-WHOLE directives — one attach semantic across
/// the exploring tools.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliberateInput {
    /// The hard question to reason through. Say in prose what you want deliberated —
    /// kaibo's explorer locates and reads the real, current code to build the dossier
    /// the offline synth then reasons over.
    pub question: String,

    /// Workspace files (under the project root) central to the question: the
    /// dossier-building explorer is directed to read each one WHOLE, so their content
    /// reaches the offline synth through the dossier. Text only — the explorer reads
    /// through the shell, so attach images to `consult` with a vision cast instead.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Absolute path to the project. Optional when the server has a default root; must
    /// be at-or-under an allowed tree (`kaibo://config` shows the set).
    #[serde(default)]
    pub path: Option<String>,

    /// Which cast runs this call; omit for the server's default. A deliberate cast pairs
    /// an interactive explorer with an OFFLINE synth (batch|direct lane) — pick from this
    /// param's `enum`; `kaibo://config` lists every cast and its lane.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the explorer (dossier-building) model id. See `kaibo://tools` for
    /// override semantics; pair with `explorer_backend` to also retarget.
    #[serde(default)]
    pub explorer_model: Option<String>,

    /// Run the explorer override on this backend (name or alias). Requires
    /// `explorer_model`. See `kaibo://tools`.
    #[serde(default)]
    pub explorer_backend: Option<String>,

    /// Override the synth (deliberating) model id. Its lane (batch|direct) still comes
    /// from the cast's synth slot. Pair with `synth_backend` to also retarget.
    #[serde(default)]
    pub synth_model: Option<String>,

    /// Run the synth override on this backend (name or alias). Requires `synth_model`.
    /// See `kaibo://tools`.
    #[serde(default)]
    pub synth_backend: Option<String>,

    /// Max tool-loop turns for the dossier-building explorer sweep (default 100).
    #[serde(default)]
    pub explorer_max_turns: Option<usize>,
}

/// The handle addressing one piece of async work — a durable batch or a
/// session-scoped background job. kaibo routes by the handle's shape.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandleInput {
    /// The handle of the async work to act on — a `backend/provider-id` batch (durable) or
    /// a `job-N` consult (this session). kaibo routes by the handle, so pass back the one
    /// you were given. See `kaibo://tools`.
    pub handle: String,
}

/// Arguments to `oneshot`. No `path`: oneshot reads no project — a thin, toolless
/// completion, so the caller owns any context the model needs.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OneshotInput {
    /// The prompt to send the model. No codebase access on this call, so include whatever
    /// context the answer needs (or `attach` files) — the model answers from this and its
    /// own knowledge.
    pub prompt: String,

    /// Workspace files to inline as context — kaibo reads them so their bytes never pass
    /// through your context. Prefer **whole files** (a tool-less model can't read the repo
    /// itself); images need a vision-capable model. See `kaibo://tools`.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Which cast (model team) runs this call; omit for the server's default. kaibo runs
    /// the cast's capable (synth) model. `kaibo://config` lists the casts.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the model id. See `kaibo://tools` for override semantics (pair with
    /// `backend` to also retarget).
    #[serde(default)]
    pub model: Option<String>,

    /// Run the `model` override on this backend (name or alias). Requires `model`.
    /// See `kaibo://tools`.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments to `batch_submit`. Many prompts, one cast/model — they all ride one
/// provider batch.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchSubmitInput {
    /// The prompts to fan out, one batch item each. Like `oneshot`, no codebase access —
    /// each prompt carries its own context (or `attach` shared files). Run at max thinking,
    /// for hard questions you'll wait on.
    pub prompts: Vec<String>,

    /// Workspace files to inline as shared context for *every* prompt — kaibo reads them so
    /// their bytes never pass through your context. Prefer **whole files**; images need a
    /// vision-capable synth model. See `kaibo://tools`.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Which cast (model team) runs the batch; omit for the server's default. Uses the
    /// cast's synth model on a batch-capable backend. `kaibo://config` lists the casts.
    #[serde(default)]
    pub cast: Option<String>,

    /// Override the synth model id — reach for it to batch a top-tier model the cast synths
    /// cheaper for interactive use. See `kaibo://tools`.
    #[serde(default)]
    pub model: Option<String>,

    /// Run the `model` override on this backend (name or alias). Requires `model`; must be
    /// batch-capable. See `kaibo://tools`.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments to `job_list`: an optional backend to scope the *batch* portion
/// of the listing to. Live consult jobs (in-memory, not backend-bound) are always
/// listed regardless.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListInput {
    /// Which backend (name or alias) to list batches from. Omit to sweep every
    /// batch-capable backend (orphan recovery for a lost handle). Does not affect the
    /// consult-jobs section, which is always shown.
    #[serde(default)]
    pub backend: Option<String>,

    /// Show *all* batches, including ones older than 24h. By default the batches section is
    /// trimmed to the last 24 hours (older ones are done and still collectible by their
    /// handle); set `all: true` for the full history. An undateable batch is always shown.
    #[serde(default)]
    pub all: bool,
}

/// Arguments to `job_wait`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {
    /// How long to block, in seconds (default 60). No clamp — your client's tool-call
    /// timeout and your ability to interrupt are the real bounds; over 3600 is a loud
    /// error. For a long park, prefer calling `job_wait` again over one giant block.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Max records to return (default 20, newest activity last).
    #[serde(default)]
    pub limit: Option<usize>,

    /// Lowest level to return: `warn` (default — what kaibo flags for you: a job finished
    /// or failed), `info` (also the watchable narrative — each kaish command, sweep,
    /// milestone), `error`, or `debug`. A salience bar, not severity.
    #[serde(default)]
    pub level: Option<String>,

    /// Optional batch handles (`backend/provider-id`) to also poll once this call, status
    /// appended. Consult jobs already surface via the activity stream. Omit to just drain
    /// it.
    #[serde(default)]
    pub handles: Vec<String>,
}

/// Arguments to `run_kaish`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunKaishInput {
    /// The kaish (sh-like) script to run against the read-only project.
    pub script: String,

    /// Absolute path to the project. Optional when the server has a default root; must be
    /// at-or-under an allowed tree (`kaibo://config` shows the set). Each call starts fresh
    /// at this root — there is no persistent cwd across calls.
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
    /// In-flight + collectable async consultations (`consult_submit`, collected via the
    /// shared `job_get`/`job_cancel`/`job_list`). Same `Arc<Mutex<LruCache>>` shape as
    /// `sessions`, so the per-request handler clones all share one registry (see
    /// [`JobStore`]).
    jobs: JobStore,
    /// The pull-side notification ring the `job_wait` tool drains — the same kaibo-target
    /// records the `mcp_log` bridge streams to the client, teed for on-demand pull.
    /// `new` seeds an unwired default (nothing pushes to it); `main` swaps in the shared
    /// ring via [`with_notifications`](Self::with_notifications) so the bridge layer feeds
    /// it. `Clone` shares one ring (see [`NotificationBuffer`](crate::mcp_log::NotificationBuffer)).
    notifications: crate::mcp_log::NotificationBuffer,
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
    /// How the batch handlers build provider clients — the injection seam. `new` seeds
    /// [`LiveBatchProviders`](crate::batch::LiveBatchProviders) (the real network
    /// builders); tests swap in a scripted double via
    /// [`with_batch_providers`](Self::with_batch_providers) to exercise the submit/poll
    /// handler wiring offline. `Arc<dyn …>` so the derived `Clone` shares one factory.
    batch_providers: Arc<dyn crate::batch::BatchProviderFactory>,
}

/// One `CAST_ENUM_RULES` entry: the tools sharing a cast eligibility, and the predicate
/// (a `Config::cast_is_*`/`cast_can_*`) that decides which usable casts they advertise.
type CastEnumRule = (&'static [&'static str], fn(&Config, &str) -> bool);

/// The single source mapping each cast-taking tool to the predicate that decides which
/// *usable* casts its `cast` enum advertises — keyed on the cast's shape (synth lane +
/// explorer, via the `Config::cast_is_*`/`cast_can_*` predicates). [`KaiboHandler::new`]
/// injects the enums straight from this table. A cast may match more than one rule (a
/// deliberate-shaped batch cast like `fable` serves both `batch_submit` and `deliberate`);
/// the rules are independent filters, not a partition.
///
/// Two tests guard it: `cast_enum_never_advertises_a_gated_cast` (no enum offers a cast its
/// tool's gate — `reject_offline_cast`/`require_batch_cast`/`require_deliberate_cast` —
/// would reject) and `every_cast_taking_tool_has_an_enum_rule` (no cast-taking tool ships
/// without a rule, i.e. a silently-empty enum). `casts_section` (the handshake roster) is a
/// *consumer* of the same `Config` predicates, not bound to this table: it renders a
/// budget-limited display subset (it hides `Direct` casts) — a presentation choice distinct
/// from tool eligibility.
const CAST_ENUM_RULES: &[CastEnumRule] = &[
    (
        &["consult", "consult_submit", "oneshot"],
        Config::cast_is_interactive,
    ),
    // `explore` runs only the explorer, so it advertises *any* cast with one — including
    // `deliberate`/`direct` casts, whose (often smarter) explorers are useful standalone.
    // Its own rule, broader than the interactive tools' (which also need an interactive synth).
    (&["explore"], Config::cast_can_explore),
    (&["batch_submit"], Config::cast_is_batch),
    (&["deliberate"], Config::cast_can_deliberate),
];

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
            // `consult_submit` is the async sibling of `consult` — same capability, a
            // submit/collect surface rather than a blocking one — so it shares the
            // `consult` gate. (docs/issues.md tracks a dedicated flag if anyone needs
            // only one shape.) The verbs that *collect* its handles (`job_get`/
            // `job_cancel`/`job_list`) are shared with batch and gated below.
            (gating.consult, "consult_submit"),
            // The single-phase explorer sweep — its own gate.
            (gating.explore, "explore"),
            // `deliberate` starts an offline deliberation; its own gate. The collect
            // verbs it hands off to (batch or job) live below, gated by their capability.
            (gating.deliberate, "deliberate"),
            (gating.oneshot, "oneshot"),
            (gating.run_kaish, "run_kaish"),
            // `--no-batch` drops `batch_submit`; the shared collect verbs live below.
            (gating.batch, "batch_submit"),
            // `job_get`/`job_cancel`/`job_list`/`job_wait` manage *both* batch and
            // consult handles — and now `deliberate` handles too (batch on its batch
            // lane, `job-N` on its direct lane) — so they stay as long as *any* of the
            // three producers is on, and drop only when all are gated off. Routing by
            // handle shape inside each verb refuses a handle whose producer is disabled.
            (
                gating.batch || gating.consult || gating.deliberate,
                "job_get",
            ),
            (
                gating.batch || gating.consult || gating.deliberate,
                "job_cancel",
            ),
            (
                gating.batch || gating.consult || gating.deliberate,
                "job_list",
            ),
            (
                gating.batch || gating.consult || gating.deliberate,
                "job_wait",
            ),
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
        // Advertise each tool's `cast` enum from the one `CAST_ENUM_RULES` table: a cast's
        // shape (synth lane + explorer) decides which tools it serves, and each rule is an
        // independent filter (the batch and deliberate views OVERLAP — a deliberate-shaped
        // batch cast like `fable` serves both). Routing every enum through this table (and
        // cross-checking it against the gates in the consistency test) is what keeps the
        // advertised menu and the call-time gate from drifting apart. The `cast` enum is the
        // one authoritative per-lane roster — the resident-prose roster this used to also
        // append (`append_cast_roster`) was dropped as redundant; see `docs/devlog.md`.
        for (tools, eligible) in CAST_ENUM_RULES {
            let casts: Vec<String> = usable
                .iter()
                .filter(|n| eligible(&config, n))
                .cloned()
                .collect();
            inject_cast_enum(&mut tool_router, tools, &casts);
        }

        // Pin `consult` resident under Claude Code's tool-schema deferral: a host may
        // defer every tool's schema to names-only until first use, but a `_meta`
        // `anthropic/alwaysLoad: true` opts a tool out. `consult` is kaibo's front
        // door — pinning it means the calling model still sees its description (what
        // it does, when to reach for it instead) with no lookup round-trip, even on a
        // host that defers everything else. Narrow and explicit: only `consult` is
        // pinned, so an unused `oneshot`/`run_kaish`/etc. still bills nothing until
        // the caller actually reaches for it. A no-op if `--no-consult` already
        // dropped the route.
        if let Some(route) = tool_router.map.get_mut("consult") {
            let mut meta = Meta::new();
            meta.insert(
                "anthropic/alwaysLoad".to_string(),
                serde_json::Value::Bool(true),
            );
            route.attr.meta = Some(meta);
        }

        let sessions = SessionStore::new(config.defaults.session_capacity);
        // Async-consult jobs get their own cap: a held job result (answer + optional
        // report) is heavier than a session's lean Q&A pair, so `job_capacity` is a
        // separate, smaller knob (`[defaults] job_capacity` / `KAIBO_JOB_CAPACITY`).
        let jobs = JobStore::new(config.defaults.job_capacity);
        Ok(Self {
            config: Arc::new(config),
            tool_router,
            tool_schemas: Arc::new(builtin_schemas()?),
            sessions,
            jobs,
            // An unwired default — nothing pushes to it until `main` swaps in the shared
            // ring the bridge layer feeds (see `with_notifications`). So a handler built
            // for a test has a valid, empty buffer and `job_wait` simply drains nothing.
            notifications: crate::mcp_log::NotificationBuffer::new(512),
            mcp_log_level: Arc::new(AtomicU8::new(mcp_log::rank(mcp_log::DEFAULT_LEVEL))),
            allowed_set: Arc::new(allowed),
            default_root: Arc::new(default_root),
            default_root_inferred,
            // The real network-client builders; tests swap in a scripted double.
            batch_providers: Arc::new(crate::batch::LiveBatchProviders),
        })
    }

    /// Swap in the shared notification ring `main` also handed the bridge layer, so the
    /// `job_wait` tool drains the records the layer pushes. A builder (not a `new` param) to
    /// keep `new(config)` unchanged for the many call sites and tests.
    pub fn with_notifications(mut self, buffer: crate::mcp_log::NotificationBuffer) -> Self {
        self.notifications = buffer;
        self
    }

    /// Swap in a batch-provider factory — the seam that lets tests drive the batch
    /// handlers (`batch_submit`, `deliberate`'s batch lane, the `job_*` batch arms) with a
    /// scripted double instead of real network clients. A builder, like
    /// [`with_notifications`](Self::with_notifications), so `new(config)` stays unchanged.
    #[cfg(test)]
    pub fn with_batch_providers(
        mut self,
        providers: Arc<dyn crate::batch::BatchProviderFactory>,
    ) -> Self {
        self.batch_providers = providers;
        self
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

    /// The allowed tree (or followed-worktree root) that contains `canon`, or `None` when
    /// it's outside the boundary — the *which-tree* sibling of [`contained`](Self::contained)
    /// (which is just `is_some()` on this). A static `allow_path` wins over a followed
    /// worktree, matching the precedence in `contained`'s original form. The returned root
    /// is what an attachment read mounts a read-only kaish worker at, so the VFS refuses a
    /// symlink escaping *that* tree (see [`resolve_attachments`](Self::resolve_attachments)).
    fn containing_tree(&self, canon: &std::path::Path) -> Option<PathBuf> {
        if let Some(tree) = self.allowed_set.iter().find(|tree| canon.starts_with(tree)) {
            return Some(tree.clone());
        }
        if self.config.follow_worktrees {
            for tree in self.allowed_set.iter() {
                if let Some(common) = crate::worktree::common_git_dir(tree) {
                    if let Some(wt) = crate::worktree::vouched_worktrees(&common)
                        .into_iter()
                        .find(|wt| canon.starts_with(wt))
                    {
                        return Some(wt);
                    }
                }
            }
        }
        None
    }

    /// The shared "outside the allowed set" rejection, naming the boundary and the three
    /// widening knobs. Used wherever [`contained`](Self::contained) says no, so the
    /// caller always learns where the edge is and how to move it.
    fn containment_error(&self, raw: &std::path::Path, canon: &std::path::Path) -> McpError {
        let trees: Vec<String> = self
            .allowed_set
            .iter()
            .map(|p| p.display().to_string())
            .collect();
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

    /// Resolve caller-named `consult`/`explore` attachments: attach means *the model
    /// sees the bytes*. A text file within `budget` (cumulative, caller order) is read
    /// server-side and inlined into the driver prompt; a text file past it is demoted to
    /// a named path the prompt directs the model to read WHOLE through its shell —
    /// loudly, never a silent drop. An image is routed to `view_image` (never inlined
    /// here). Each path must canonicalize to a regular file *under `root`* (returned
    /// root-relative, `cat`'d from cwd) — the only real tree the consult shell mounts;
    /// anything out of reach is refused with guidance.
    ///
    /// **Inlined bytes are read through the read-only kaish VFS**, exactly like
    /// [`resolve_attachments`](Self::resolve_attachments) and for the same reason: these
    /// bytes enter the model's context, so a raced symlink-swap must be refused at the
    /// mount layer, not merely at a canonicalize-then-read check. Demoted files are
    /// never read here at all — the model's own `cat` goes through the same VFS.
    async fn resolve_consult_attachments(
        root: &std::path::Path,
        attach: &[String],
        budget: usize,
        sandbox: &crate::sandbox::SandboxConfig,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        use crate::attach::{check_attachment_bounds, DEFAULT_MAX_ATTACHMENTS};
        // Count cap only (total pinned to 0, so the byte-budget arm never fires): a
        // stray thousand-file glob is refused before any canonicalize/read work. There
        // is deliberately NO cumulative byte cap on this path — `budget` bounds
        // everything that's actually read (inlined), and a demoted file is never read
        // here at all, so there's nothing left for a cumulative cap to protect.
        check_attachment_bounds(
            attach.len(),
            0,
            DEFAULT_MAX_ATTACHMENTS,
            crate::attach::DEFAULT_MAX_TOTAL_BYTES,
        )
        .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        // One read-only worker rooted at the consult root, spawned lazily on the first
        // inlined read (a demote-everything call — budget 0, say — spawns none).
        let mut worker: Option<crate::sandbox::KaishWorker> = None;
        let mut remaining = budget as u64;
        let mut out = Vec::with_capacity(attach.len());
        for p in attach {
            let raw = std::path::PathBuf::from(p);
            // A relative path reads from the project root (where the model's shell starts).
            let joined = if raw.is_absolute() {
                raw.clone()
            } else {
                root.join(&raw)
            };
            let canon = std::fs::canonicalize(&joined).map_err(|e| {
                McpError::invalid_params(
                    format!("attached file {} could not be resolved: {e}", raw.display()),
                    None,
                )
            })?;
            let meta = std::fs::metadata(&canon).map_err(|e| {
                McpError::invalid_params(
                    format!("attached file {} could not be read: {e}", canon.display()),
                    None,
                )
            })?;
            if !meta.is_file() {
                return Err(McpError::invalid_params(
                    format!("attached file {} is not a regular file", canon.display()),
                    None,
                ));
            }
            // The consult model reads attachments through its shell, which mounts exactly
            // one real tree: the project root. A path under root is passed root-relative
            // (the model `cat`s it from cwd); anything out of reach gets a refusal naming
            // the project root.
            let display_path = if let Ok(rel) = canon.strip_prefix(root) {
                rel.display().to_string()
            } else {
                return Err(McpError::invalid_params(
                    format!(
                        "attached file {} resolves outside the project root {} — consult reads \
                         attachments through its shell, which only mounts that. Paste it into \
                         `context`, or use `oneshot`/`batch` attach (which inline a file from \
                         anywhere in the allowed set).",
                        raw.display(),
                        root.display(),
                    ),
                    None,
                ));
            };
            // Sniff the file's type by content, not extension — a 16-byte prefix covers
            // every magic number `sniff_mime` knows.
            //
            // This prefix is read with `std::fs`, NOT through the kaish VFS — a
            // deliberate, bounded exception that only ever *routes*: the 16 bytes feed
            // `sniff_mime` to a bool and are dropped, and kaibo's adversary is the
            // *model*, which has no control over filesystem timing to drive a swap. For
            // an image the authoritative read still goes through the read-only VFS in
            // `view_image` at view time (full bytes, full containment, re-sniffed); for
            // a demoted text file the model's own `cat` does. Anything we INLINE below
            // is read through the VFS and re-sniffed from its full bytes, so the worst
            // case of a raced swap here is a misrouted hint the model recovers from,
            // never an escape or a leak. An unreadable prefix simply reads as text.
            let prefix_is_image = {
                use std::io::Read;
                let mut buf = [0u8; 16];
                let n = std::fs::File::open(&canon)
                    .and_then(|mut f| f.read(&mut buf))
                    .unwrap_or(0);
                crate::view_image::sniff_mime(&buf[..n]).is_some()
            };
            if prefix_is_image {
                out.push(crate::consult::ConsultAttachment::Image { path: display_path });
                continue;
            }
            // Text past the remaining inline budget is demoted, not refused: unlike the
            // tool-less tools, this model CAN read the file itself, and the prompt
            // orders it to — whole, paged past the output cap.
            if meta.len() > remaining {
                out.push(crate::consult::ConsultAttachment::TextOversize {
                    path: display_path,
                    size: meta.len(),
                });
                continue;
            }
            // Within budget: inline. The read is VFS-mounted (see doc-comment) — these
            // bytes enter the model's context.
            if worker.is_none() {
                worker = Some(
                    crate::sandbox::KaishWorker::spawn_with(root, sandbox.clone()).map_err(
                        |e| {
                            McpError::internal_error(
                                format!("attachment reader for {}: {e:#}", root.display()),
                                None,
                            )
                        },
                    )?,
                );
            }
            let bytes = worker
                .as_ref()
                .expect("worker was just spawned")
                .read_file(canon.clone())
                .await
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("attached file {} could not be read: {e:#}", canon.display()),
                        None,
                    )
                })?;
            // Re-sniff from the full bytes — authoritative for anything inlined. A file
            // that grew past the remaining budget between stat and read demotes instead.
            if crate::view_image::sniff_mime(&bytes).is_some() {
                out.push(crate::consult::ConsultAttachment::Image { path: display_path });
                continue;
            }
            if bytes.len() as u64 > remaining {
                out.push(crate::consult::ConsultAttachment::TextOversize {
                    path: display_path,
                    size: bytes.len() as u64,
                });
                continue;
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    remaining -= bytes.len() as u64;
                    out.push(crate::consult::ConsultAttachment::Text {
                        path: display_path,
                        body: text.to_string(),
                    });
                }
                // Neither text nor image: refuse loudly — it can't be inlined honestly,
                // and the model's shell would refuse to `cat` binary too, so naming it
                // would burn a turn on a dead end.
                Err(_) => {
                    return Err(McpError::invalid_params(
                        format!(
                            "attached file {display_path} is neither valid UTF-8 text nor a \
                             recognized image (png/jpeg/gif/webp) — kaibo won't inline binary, \
                             and the model's shell can't read it either. Convert it first, or \
                             paste the relevant text into `context`."
                        ),
                        None,
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Resolve attachments for a sweep-only tool (`explore`, `deliberate`'s dossier
    /// stage): read-WHOLE directives, never inlined bytes — budget 0, so no file is
    /// read here at all. Images are refused up front: the sweep toolset carries no
    /// `view_image` (see `run_explore_phase`), so naming one would send the
    /// investigator down a dead end (`cat` refuses binary).
    async fn resolve_sweep_attachments(
        &self,
        root: &std::path::Path,
        attach: &[String],
        tool: &str,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        let attachments =
            Self::resolve_consult_attachments(root, attach, 0, &self.config.sandbox).await?;
        if let Some(img) = attachments.iter().find(|a| a.is_image()) {
            return Err(McpError::invalid_params(
                format!(
                    "attached file {} is an image, but {tool}'s investigator reads through \
                     the shell and can't view images — attach it to `consult` with a \
                     vision-capable cast instead",
                    img.path()
                ),
                None,
            ));
        }
        Ok(attachments)
    }

    /// Refuse an image attachment to a vision-blind consult synth — the consult analog of
    /// [`gate_image_attachments`]. consult never inlines an image's bytes; the model opens
    /// an attached image with the `view_image` tool, which is only wired into the toolset
    /// when the synth is vision-capable (see `consult_tools`). So an image attached to a
    /// blind synth could never be seen — refuse honestly up front, naming the cast, rather
    /// than let the prompt name a file the model has no way to open. Text attachments (and
    /// the no-attachment case) always pass.
    fn gate_consult_image_attachments(
        attachments: &[crate::consult::ConsultAttachment],
        vision: bool,
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        if !vision && attachments.iter().any(|a| a.is_image()) {
            return Err(McpError::invalid_params(
                format!(
                    "an image was attached, but the consult synth `{model}` on cast `{cast}` \
                     can't see images — consult opens an attached image with its `view_image` \
                     tool, which only a vision-capable synth carries. Use a vision-capable \
                     cast, or attach only text files. `kaibo://config` lists each slot's \
                     `vision`."
                ),
                None,
            ));
        }
        Ok(())
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
    /// ceiling gets a directory map, and one too large for even that gets a short
    /// discover-as-you-go note — never a refusal, per `OrientationConfig::assemble`.
    /// Errors here are real failures (kernel spawn, unparseable enumeration), not
    /// size. Only the *exploring* tools call this.
    async fn orientation(&self, root: &std::path::Path) -> Result<Option<Arc<str>>, McpError> {
        self.config
            .orientation
            .assemble(root, self.config.sandbox.clone())
            .await
            .map(|opt| opt.map(Arc::from))
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))
    }

    /// Resolve this call's per-phase system prompts for `cast` — the per-slot `preamble`
    /// over the global `[prompts]` table. Thin wrapper over [`Cast::resolved_prompts`]
    /// (the shared layering the `kaibo://prompts/{cast}` resource also renders); `cast`
    /// is the post-override clone, so a per-call model override (a bare slot) correctly
    /// carries no preamble.
    fn resolved_prompts(&self, cast: &Cast) -> PromptOverrides {
        cast.resolved_prompts(&self.config.prompts)
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

    /// Refuse an interactive tool (`consult`/`consult_submit`/`oneshot`) on a cast whose
    /// synth runs on an offline lane. A `Batch` synth is a big, slow, expensive model
    /// tuned for free offline batch latency (Gemini Pro, Claude Opus); a `Direct` synth
    /// is a big local model kaibo runs itself, taking the time it takes — either way,
    /// driving it through an interactive tool loop is the wrong-and-costly mistake this
    /// gate exists to stop. Points the caller at the lane that fits.
    fn reject_offline_cast(&self, cast: &Cast, tool: &str) -> Result<(), McpError> {
        match cast.synth_lane() {
            Some(Lane::Batch) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs on the `batch` lane — submit it with \
                     `batch_submit`, not `{tool}`. It's a big, slow model tuned for free \
                     offline batch latency; running it interactively would be slow and \
                     expensive. Pick an interactive cast for `{tool}`.",
                    cast.name
                ),
                None,
            )),
            Some(Lane::Direct) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs offline (`lane = \"direct\"`) — interactive \
                     tools need an interactive synth. Pick an interactive cast for `{tool}`.",
                    cast.name
                ),
                None,
            )),
            None => Ok(()),
        }
    }

    /// Refuse `batch_submit` on a cast whose synth isn't on the `batch` lane
    /// specifically — the other half of the lane split. A batch cast must positively
    /// declare `lane = "batch"` on its synth slot, so an ordinary interactive cast is
    /// never silently batched (an accidental Opus/Pro batch is just as costly the other
    /// way), and a `direct` cast — offline, but not batch — gets its own honest
    /// message rather than the generic "not a batch cast" one. Points the caller at
    /// the built-in batch casts.
    fn require_batch_cast(&self, cast: &Cast) -> Result<(), McpError> {
        match cast.synth_lane() {
            Some(Lane::Batch) => Ok(()),
            Some(Lane::Direct) => Err(McpError::invalid_params(
                format!(
                    "cast `{}`'s synth runs on the `direct` lane, not `batch` — \
                     `batch_submit` needs a synth slot with `lane = \"batch\"`.",
                    cast.name
                ),
                None,
            )),
            None => Err(McpError::invalid_params(
                format!(
                    "cast `{}` is not a batch cast — `batch_submit` needs a cast whose synth \
                     slot declares `lane = \"batch\"` (the built-ins `gemini-batch`, \
                     `anthropic-batch`, or your own in config.toml). For an interactive \
                     answer, use `consult`/`oneshot`.",
                    cast.name
                ),
                None,
            )),
        }
    }

    /// Refuse `deliberate` on a cast without an **offline synth**. deliberate =
    /// explore → offline synth, so the synth must run on the batch or direct lane
    /// (an interactive synth belongs to `consult`). The other half — a *missing
    /// explorer* — is caught when the explorer arm is resolved (`arm` errors, naming
    /// the gap), the same way `explore` leans on it; here we only need the synth-lane
    /// half. A synth-only batch cast (`anthropic-batch`) passes this but fails at the
    /// explorer resolve, which is the honest error (no dossier phase to staff).
    fn require_deliberate_cast(&self, cast: &Cast) -> Result<(), McpError> {
        match cast.synth_lane() {
            Some(_) => Ok(()),
            None => Err(McpError::invalid_params(
                format!(
                    "cast `{}` has no offline synth — `deliberate` needs a cast pairing an \
                     interactive explorer with a synth on the `batch` or `direct` lane (the \
                     example config's `fable`/`gemini-deliberate`/`local-direct`, or your \
                     own). For an answer this turn, use `consult`.",
                    cast.name
                ),
                None,
            )),
        }
    }

    /// Resolve `deliberate`'s offline lane and apply the per-call model overrides, in the
    /// one order that's correct. The lane is captured from the *chosen cast* **before** the
    /// overrides run, because `apply_model_override` replaces a slot with a *bare* (laneless)
    /// one — an override retargets the model, never the offline mechanism — so reading
    /// `synth_lane()` afterward would silently lose batch|direct (and hit the `.expect`).
    /// Assumes [`require_deliberate_cast`](Self::require_deliberate_cast) already passed, so
    /// the synth lane is `Some`; the sole caller enforces that first. Returns the captured
    /// lane; `cast` carries the overrides on return.
    fn deliberation_lane_with_overrides(
        &self,
        cast: &mut Cast,
        explorer_model: Option<&str>,
        explorer_backend: Option<&str>,
        synth_model: Option<&str>,
        synth_backend: Option<&str>,
    ) -> Result<Lane, McpError> {
        let lane = cast
            .synth_lane()
            .expect("require_deliberate_cast guaranteed an offline synth");
        self.apply_model_override(
            cast,
            ModelRole::Explorer,
            explorer_model,
            explorer_backend,
            "explorer_model",
            "explorer_backend",
        )?;
        self.apply_model_override(
            cast,
            ModelRole::Synth,
            synth_model,
            synth_backend,
            "synth_model",
            "synth_backend",
        )?;
        Ok(lane)
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

    #[tool(
        description = "Ask a model outside your own family about a codebase — code review, \
            debugging, architecture, \"what does this change break\" — and get a grounded \
            answer with `file:line` citations. A capable model (DeepSeek, Gemini, \
            Anthropic, or local — pick with `cast`) drives a READ-ONLY shell over the \
            project: it reads the real, current source, delegates broad sweeps to a fast \
            explorer, and answers with evidence, never modifying anything. Describe your \
            intent in prose; kaibo locates the code itself, so you don't paste files or \
            diffs. `attach` puts specific files in front of it; `session_id` threads a \
            multi-turn consultation. For a toolless opinion use `oneshot`; to run in the \
            background use `consult_submit`."
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
        self.reject_offline_cast(&cast, "consult")?;
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
        // Resolve attachments (inline within budget, demote past it, classify images),
        // then gate: an image needs a vision-capable synth (consult views it with
        // `view_image`, which only a vision synth carries). Refuse here, before the
        // loop, the same honest up-front refusal oneshot/batch give.
        let attachments = Self::resolve_consult_attachments(
            &root,
            &input.attach,
            defaults.inline_attach_budget,
            &self.config.sandbox,
        )
        .await?;
        Self::gate_consult_image_attachments(
            &attachments,
            synth.caps.vision,
            &synth.model,
            &cast.name,
        )?;
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    progress: progress.clone(),
                    house_rules: self.house_rules(&root)?,
                    prompts: self.resolved_prompts(&cast),
                    orientation: self.orientation(&root).await?,
                    call_deadline: defaults.call_deadline,
                },
                explorer_max_turns: input
                    .explorer_max_turns
                    .unwrap_or(defaults.explorer_max_turns),
                sandbox: self.config.sandbox.clone(),
            },
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            attachments,
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
        description = "Run a `consult` in the background: same read-only investigation, \
            same arguments, but returns a `job-N` handle immediately. Fan out a \
            cross-model study (one submit per cast, collect them all) or keep working \
            while a deep consult runs. `job_wait` parks for results, `job_get` fetches \
            them, `job_cancel` stops one. Handles live for this server session only. \
            For an answer in this turn, use `consult`."
    )]
    async fn consult_submit(
        &self,
        Parameters(input): Parameters<ConsultInput>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        // Resolve cast + per-call overrides + arms exactly as `consult` does — all the
        // refusable work (bad cast, bad path, missing key) happens *here*, synchronously,
        // so a bad submit is a clean error, not a job that fails on poll.
        let mut cast = self.resolve_cast(input.cast)?;
        self.reject_offline_cast(&cast, "consult_submit")?;
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
        let defaults = &self.config.defaults;
        // Resolve + classify + gate before spawning: a bad attach (or an image to a blind
        // synth) is a clean up-front refusal, not a job that fails on poll.
        let attachments = Self::resolve_consult_attachments(
            &root,
            &input.attach,
            defaults.inline_attach_budget,
            &self.config.sandbox,
        )
        .await?;
        Self::gate_consult_image_attachments(
            &attachments,
            synth.caps.vision,
            &synth.model,
            &cast.name,
        )?;
        // An async job has no live MCP peer to push progress notifications to, so route
        // its liveness onto the `tracing` stream: the `mcp_log` bridge mirrors it to a
        // watching client (the live view sync `consult` had) and the notification buffer
        // tees it for `job_wait`. The `ProgressLog` decorator wraps that `TracingSink` so
        // the job *also* remembers the latest beat — `job_get`/`job_list` echo it inline,
        // a second channel for a poller who isn't using `job_wait`. The job below keeps a
        // clone of this exact handle, so what it reads is what the running phase emitted.
        let progress_log = Arc::new(ProgressLog::new(Arc::new(TracingSink)));
        let cfg = ConsultConfig {
            explore: ExploreConfig {
                phase: PhaseContext {
                    progress: progress_log.clone(),
                    house_rules: self.house_rules(&root)?,
                    prompts: self.resolved_prompts(&cast),
                    orientation: self.orientation(&root).await?,
                    call_deadline: defaults.call_deadline,
                },
                explorer_max_turns: input
                    .explorer_max_turns
                    .unwrap_or(defaults.explorer_max_turns),
                sandbox: self.config.sandbox.clone(),
            },
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            attachments,
        };

        // Owned captures for the `'static` spawned task. The session store is `Clone`
        // (an `Arc` inside), so the task holds its own handle and rebuilds the borrow
        // (`&store, &id`) inside the async block where both live.
        let question = input.question.clone();
        let context = input.context.clone();
        let sessions = self.sessions.clone();
        let session_id = input.session_id.clone();
        let include_report = input.include_report;
        let cast_name = cast.name.clone();
        let explorer_model = explorer.model.clone();
        let synth_model = synth.model.clone();
        let label =
            format!("cast `{cast_name}` (explorer `{explorer_model}`, synth `{synth_model}`)");

        let job_id = self.jobs.submit(label, progress_log, async move {
            let session = session_id.as_ref().map(|id| (&sessions, id.as_str()));
            match consult(
                &question,
                context.as_deref(),
                root,
                &explorer,
                &synth,
                &cfg,
                session,
            )
            .await
            {
                Ok(out) => {
                    let answer = with_provenance(
                        out.answer,
                        &cast_name,
                        &[
                            ("explorer", explorer_model.as_str()),
                            ("synth", synth_model.as_str()),
                        ],
                    );
                    Ok(JobResult {
                        answer,
                        report: include_report.then_some(out.report),
                    })
                }
                // Render the failure to its final text here (classification + guidance),
                // so `job_get` wraps a ready string without re-deriving anything.
                Err(e) => Err(consultation_failure_text("consult", &cast_name, e)),
            }
        });

        let msg = format!(
            "Submitted consultation `{job_id}` on cast `{}`. It runs in the \
             background — go do other work and `job_get {job_id}` for the answer; \
             `job_cancel {job_id}` stops it. Nothing to wait on now.",
            cast.name
        );
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Survey a codebase and get back a structured, cited report — not an \
            answer. A fast, cheap model sweeps the project READ-ONLY (grep, whole-file \
            reads) and returns a summary of findings, the relevant locations with \
            `file:line`, and the trail it followed. `attach` names text files it must \
            read whole during the sweep. The evidence-gathering half of `consult`, \
            exposed directly: map unfamiliar code, or assemble a cited survey to reason \
            over yourself. For a synthesized answer instead, use `consult`."
    )]
    async fn explore(
        &self,
        Parameters(input): Parameters<ExploreInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        // Resolve the cast, then layer a per-call explorer override onto the clone.
        // Deliberately NO `reject_offline_cast`: explore runs the *explorer* arm
        // interactively, so a deliberate/direct cast's explorer is perfectly valid —
        // explore only needs an explorer slot, resolved next (a synth-only batch cast
        // has none and `arm` errors clearly).
        let mut cast = self.resolve_cast(input.cast)?;
        self.apply_model_override(
            &mut cast,
            ModelRole::Explorer,
            input.explorer_model.as_deref(),
            input.explorer_backend.as_deref(),
            "explorer_model",
            "explorer_backend",
        )?;
        let explorer = self.arm(&cast, ModelRole::Explorer)?;
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let attachments = self
            .resolve_sweep_attachments(&root, &input.attach, "explore")
            .await?;
        let cfg = ExploreConfig {
            phase: PhaseContext {
                progress: progress.clone(),
                house_rules: self.house_rules(&root)?,
                prompts: self.resolved_prompts(&cast),
                orientation: self.orientation(&root).await?,
                call_deadline: defaults.call_deadline,
            },
            explorer_max_turns: input
                .explorer_max_turns
                .unwrap_or(defaults.explorer_max_turns),
            sandbox: self.config.sandbox.clone(),
        };

        let span =
            tracing::info_span!("explore", cast = %cast.name, explorer_model = %explorer.model);
        progress.emit(PhaseEvent::PhaseStarted { phase: "explore" });
        let report = match explore_with(&input.question, root, &explorer, &cfg, &attachments)
            .instrument(span)
            .await
        {
            Ok(report) => report,
            // A provider/model-loop failure is a clean tool-result error, same as `consult`.
            Err(e) => return Ok(consultation_failed("explore", &cast.name, e)),
        };
        progress.emit(PhaseEvent::PhaseFinished { phase: "explore" });

        // The report IS the text (no structured_content). Provenance names the one arm
        // that produced it, so a cross-model study sees which explorer surveyed.
        let report = with_provenance(report, &cast.name, &[("explorer", &explorer.model)]);
        Ok(CallToolResult::success(vec![Content::text(report)]))
    }

    #[tool(
        description = "Put a top model's deepest reasoning on your codebase without holding \
            a session open. A fast model first investigates the project READ-ONLY and \
            assembles a cited dossier (you wait for this — minutes); a heavyweight synth \
            then deliberates offline over that evidence — a frontier model on the \
            provider's batch lane (max thinking, half price) or a big local model taking \
            the time it takes. Returns a durable handle once the dossier is built; keep \
            working, then `job_wait`/`job_get` it. Best for hard questions worth hours — a \
            design review, a gnarly bug, \"is this abstraction right\". For an answer this \
            turn, use `consult`."
    )]
    async fn deliberate(
        &self,
        Parameters(input): Parameters<DeliberateInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let root = self.resolve_root(input.path)?;
        let mut cast = self.resolve_cast(input.cast)?;
        // deliberate = explore → OFFLINE synth. Require the synth on an offline lane
        // here; the other half — a present, interactive explorer — is enforced when the
        // explorer arm resolves below (a synth-only batch cast has no explorer slot and
        // `arm` errors clearly, the honest "no dossier phase to staff" refusal).
        self.require_deliberate_cast(&cast)?;
        // Capture the lane and apply per-call overrides in the one correct order (the
        // capture must precede the overrides — see the helper). Extracted so a test can
        // pin that a `synth_model` override never drops batch|direct.
        let lane = self.deliberation_lane_with_overrides(
            &mut cast,
            input.explorer_model.as_deref(),
            input.explorer_backend.as_deref(),
            input.synth_model.as_deref(),
            input.synth_backend.as_deref(),
        )?;
        let explorer = self.arm(&cast, ModelRole::Explorer)?;
        let explorer_model = explorer.model.clone();

        // Stage 1 — build the dossier synchronously, on the live progress sink: the
        // caller waits through this bounded (minutes) explorer sweep, exactly as `explore`
        // does, so a thin/failed dossier is a clean error *before* any offline tokens are
        // spent. Only the deliberation (Stage 2) is handed off async. Attachments reach
        // the dossier-builder as read-WHOLE directives (the sweep semantics), so their
        // content flows to the offline synth through the dossier it writes.
        let progress = progress_sink(peer, &meta);
        let defaults = &self.config.defaults;
        let attachments = self
            .resolve_sweep_attachments(&root, &input.attach, "deliberate")
            .await?;
        let cfg = ExploreConfig {
            phase: PhaseContext {
                progress: progress.clone(),
                house_rules: self.house_rules(&root)?,
                prompts: self.resolved_prompts(&cast),
                orientation: self.orientation(&root).await?,
                call_deadline: defaults.call_deadline,
            },
            explorer_max_turns: input
                .explorer_max_turns
                .unwrap_or(defaults.explorer_max_turns),
            sandbox: self.config.sandbox.clone(),
        };
        let span = tracing::info_span!("deliberate.dossier", cast = %cast.name, explorer_model = %explorer_model);
        progress.emit(PhaseEvent::PhaseStarted {
            phase: "deliberate.dossier",
        });
        let dossier = match explore_with(&input.question, root, &explorer, &cfg, &attachments)
            .instrument(span)
            .await
        {
            Ok(d) => d,
            Err(e) => return Ok(consultation_failed("deliberate", &cast.name, e)),
        };
        progress.emit(PhaseEvent::PhaseFinished {
            phase: "deliberate.dossier",
        });

        // Stage 2 — hand the dossier to the offline synth. Its lane picks the mechanism
        // and the handle; both share the offline-synth preamble (`batch_system_prompt`,
        // overridable via `[prompts].batch` OR the synth slot's own `preamble` — the
        // resolved `cfg.phase.prompts` already layered both, same as the dossier phase above).
        let system = crate::consult::batch_system_prompt(cfg.phase.prompts.batch.as_deref());
        match lane {
            Lane::Batch => {
                self.deliberate_batch(&cast, &explorer_model, &input.question, &dossier, &system)
                    .await
            }
            Lane::Direct => self.deliberate_direct_job(
                &cast,
                &explorer_model,
                &input.question,
                &dossier,
                &system,
            ),
        }
    }

    /// Stage 2, batch lane: submit the dossier+question as a one-item provider batch
    /// (max thinking, half price) and hand back the durable `backend/provider-id` handle.
    /// The dossier phase already ran, so this is only the submit — reusing the same
    /// `batch::submitter` + shaping `batch_submit` uses, minus the vision gate (a
    /// deliberate `attach` reaches the dossier stage as read-whole directives, so this
    /// submit carries no attachment parts; the dossier is text in the item prompt).
    async fn deliberate_batch(
        &self,
        cast: &Cast,
        explorer_model: &str,
        question: &str,
        dossier: &str,
        system: &str,
    ) -> Result<CallToolResult, McpError> {
        let (slot, backend, _caps) = self.batch_synth(cast)?;
        let backend_name = backend.name.clone();
        let model = slot.id.clone();
        let provider = self
            .batch_providers
            .submitter(backend, slot, &self.config.defaults)
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        let items = vec![crate::batch::BatchItem {
            custom_id: "0".to_string(),
            prompt: crate::consult::deliberation_prompt(question, dossier),
        }];
        let span = tracing::info_span!("deliberate.batch", cast = %cast.name, model = %model);
        let provider_id = provider
            .submit(system, &[], &items)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let handle = format!("{backend_name}/{provider_id}");
        let msg = format!(
            "Dossier built (explorer `{explorer_model}`) and handed to the batch lane as \
             `{handle}` — cast `{}`, synth `{model}` at max thinking. It deliberates offline; \
             collect it with `job_get {handle}` (durable — survives restart), or stop it with \
             `job_cancel {handle}`. Nothing to wait on now.",
            cast.name
        );
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    /// Stage 2, direct lane: spawn a session-scoped `job-N` that runs the big LOCAL synth
    /// as one long toolless completion over the dossier. No provider handle exists on this
    /// lane, so the job stays `job-N` end to end (said loudly in the reply — a restart
    /// loses it, matching the standing no-daemon decision). Mirrors `consult_submit`'s
    /// spawn, but the background work is `deliberate_direct`, not the consult loop.
    fn deliberate_direct_job(
        &self,
        cast: &Cast,
        explorer_model: &str,
        question: &str,
        dossier: &str,
        system: &str,
    ) -> Result<CallToolResult, McpError> {
        let synth = self.arm(cast, ModelRole::Synth)?;
        let synth_model = synth.model.clone();
        // deliberate-direct is exactly ONE long completion, so its wall-clock backstop
        // tracks the *synth backend's* own `request_timeout` (which the operator already
        // tunes for a slow local model) rather than the interactive `call_deadline` — a
        // slow deliberate must not force the interactive-loop ceiling high. The margin
        // above `request_timeout` lets the per-request reqwest deadline fire first (a
        // cleaner error); this tokio timer is the backstop for when it doesn't.
        let synth_slot = cast
            .require_slot(ModelRole::Synth)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let synth_backend = self
            .config
            .resolve_backend(&synth_slot.backend)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let deadline = deliberate_direct_deadline(synth_backend);
        // Same progress plumbing as consult_submit: a job has no live peer, so route
        // liveness onto `tracing` and let the ProgressLog remember the latest beat for
        // `job_get`/`job_list`. The sink handed to the phase is the same Arc the job
        // snapshots, so what it reads is what the completion emitted.
        let progress_log = Arc::new(ProgressLog::new(Arc::new(TracingSink)));
        let sink: Arc<dyn ProgressSink> = progress_log.clone();
        let cast_name = cast.name.clone();
        let explorer_model = explorer_model.to_string();
        let question = question.to_string();
        let dossier = dossier.to_string();
        let system = system.to_string();
        let label = format!("cast `{cast_name}` deliberate (direct synth `{synth_model}`)");

        let job_id = self.jobs.submit(label, progress_log, async move {
            match crate::consult::deliberate_direct(
                &question, &dossier, &synth, &system, deadline, &sink,
            )
            .await
            {
                Ok(answer) => Ok(JobResult {
                    answer: with_provenance(
                        answer,
                        &cast_name,
                        &[
                            ("explorer", explorer_model.as_str()),
                            ("synth", synth_model.as_str()),
                        ],
                    ),
                    report: None,
                }),
                Err(e) => Err(consultation_failure_text("deliberate", &cast_name, e)),
            }
        });

        let msg = format!(
            "Dossier built; the direct (local) synth is now deliberating offline as \
             `{job_id}` — cast `{}`. This is one long local completion (it can take a \
             while): `job_wait {job_id}` parks for it, `job_get {job_id}` collects, \
             `job_cancel {job_id}` stops it. Session-scoped — the job lives for this \
             server session only.",
            cast.name
        );
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Ask a model outside your own family a direct question — prompt in, \
            answer out. No tools, no codebase access: the second-opinion primitive for \
            when you already own the context. Paste what's needed, or `attach` whole \
            files (kaibo inlines them, so their bytes never cross your context). Pick \
            the answering team with `cast`. When kaibo should investigate the code \
            itself, use `consult`; to fan many prompts offline at batch prices, use \
            `batch_submit`."
    )]
    async fn oneshot(
        &self,
        Parameters(input): Parameters<OneshotInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let mut cast = self.resolve_cast(input.cast)?;
        self.reject_offline_cast(&cast, "oneshot")?;
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
        let attachments = self.resolve_attachments(&input.attach).await?;
        // Gate image attachments on the model's vision capability (shared with batch).
        self.gate_image_attachments(arm.caps.vision, &attachments, &arm.model, &cast.name)?;
        let progress = progress_sink(peer, &meta);
        let cfg = PhaseContext {
            progress: progress.clone(),
            // oneshot reads no project: no house rules, no repo map, no shell.
            house_rules: None,
            prompts: self.resolved_prompts(&cast),
            orientation: None,
            call_deadline: self.config.defaults.call_deadline,
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
        description = "Run a kaish (sh-like) script against the READ-ONLY project; \
            returns exit code + stdout + stderr. Read generously with line numbers — \
            `cat -n FILE` for a whole file, `grep -rn PATTERN .` to locate across \
            files — and compose builtins with pipes (grep/jq/awk/find/...). Writes \
            and external commands are refused (exit 126 = blocked, 124 = timed out); \
            each call starts fresh at the project root. See `kaibo://kaish/*` (or \
            `help` in the script) for idioms and the bash habits that don't carry over."
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
        self.batch_providers
            .poller(backend)
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))
    }

    /// The set of backend names `job_list` should query. An explicit `backend` scopes
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
        description = "Fan self-contained prompts to a top-tier model on the provider's \
            batch lane — offline, max thinking, half price. Like `oneshot`, no tools \
            and no codebase access: each prompt carries its own context, or `attach` \
            files shared by all. Returns a durable `backend/provider-id` handle that \
            survives restarts: submit, go work, then `job_wait`/`job_get`. Needs a \
            batch-capable cast/backend (you get a clear refusal naming them otherwise)."
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
        self.require_batch_cast(&cast)?;
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
        let attachments = self.resolve_attachments(&input.attach).await?;
        // Gate image attachments on the synth model's vision capability before the
        // provider is built — so a vision misconfig needs no key to report.
        self.gate_image_attachments(caps.vision, &attachments, &model, &cast.name)?;
        // Now build the network client (resolves the key); a batch-incapable backend is
        // refused honestly here.
        let provider = self
            .batch_providers
            .submitter(backend, slot, &self.config.defaults)
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
        // `[prompts].batch` OR the synth slot's own `preamble` (resolved together here).
        // Reads no project (no map / house rules), like oneshot.
        let system =
            crate::consult::batch_system_prompt(self.resolved_prompts(&cast).batch.as_deref());
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
             Poll it with `job_get {handle}` (it'll show progress, then per-item answers \
             when done); stop it with `job_cancel {handle}`.",
            items.len(),
            cast.name,
            model
        );
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Collect async work by handle — a batch (`backend/provider-id`) \
            or a background job (`job-N`). Returns a progress line while it runs, the \
            full result once done (batches: every item's answer, per-item failures \
            surfaced). Collect occasionally rather than in a tight loop — nothing is \
            lost by waiting."
    )]
    async fn job_get(
        &self,
        Parameters(input): Parameters<HandleInput>,
    ) -> Result<CallToolResult, McpError> {
        if is_batch_handle(&input.handle) {
            self.ensure_batch_enabled(&input.handle)?;
            let (backend_name, provider_id) = parse_batch_handle(&input.handle)?;
            let provider = self.batch_poller(backend_name)?;
            let span = tracing::info_span!("job_get", handle = %input.handle);
            let poll = provider
                .poll(provider_id)
                .instrument(span)
                .await
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
            let label = format!("{backend_name} · {provider_id}");
            return Ok(CallToolResult::success(vec![Content::text(
                crate::batch::render_poll(&poll, &label),
            )]));
        }
        self.ensure_consult_enabled(&input.handle)?;
        match self.jobs.get(&input.handle) {
            Some(snap) => {
                // Collecting a terminal job retires its completion ping from the
                // `job_wait` ring — otherwise that stale Warn lingers and the next
                // `job_wait` returns on it immediately instead of blocking for new work.
                // A Running job has no ping yet; a Canceled one never emitted one — both
                // are no-ops, so this is safe to call unconditionally, but we scope it to
                // the terminal states the ping actually exists for.
                if matches!(snap.state, JobState::Done(_) | JobState::Failed(_)) {
                    self.notifications.discard_job_pings(&input.handle);
                }
                Ok(render_job(&input.handle, snap))
            }
            None => Err(McpError::invalid_params(
                format!(
                    "no consultation job `{}` — it may have finished and been evicted by \
                     newer submits, been canceled, or never existed. Consult job ids look \
                     like `job-1` and live only for this server session.",
                    input.handle
                ),
                None,
            )),
        }
    }

    #[tool(
        description = "Stop a running async job by handle — a batch stops scheduling \
            new items (in-flight ones finish); a background job aborts its \
            investigation. `job_get` it afterward for the final state. A job that \
            already finished is left alone."
    )]
    async fn job_cancel(
        &self,
        Parameters(input): Parameters<HandleInput>,
    ) -> Result<CallToolResult, McpError> {
        if is_batch_handle(&input.handle) {
            self.ensure_batch_enabled(&input.handle)?;
            let (backend_name, provider_id) = parse_batch_handle(&input.handle)?;
            let provider = self.batch_poller(backend_name)?;
            let span = tracing::info_span!("job_cancel", handle = %input.handle);
            provider
                .cancel(provider_id)
                .instrument(span)
                .await
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Requested cancellation of batch `{}`. `job_get` it for the final \
                 per-item results.",
                input.handle
            ))]));
        }
        self.ensure_consult_enabled(&input.handle)?;
        let msg = match self.jobs.cancel(&input.handle) {
            CancelOutcome::Canceled => format!("Canceled consultation `{}`.", input.handle),
            CancelOutcome::AlreadyFinished => format!(
                "Consultation `{}` had already finished — `job_get` it for the answer.",
                input.handle
            ),
            CancelOutcome::Unknown => {
                return Err(McpError::invalid_params(
                    format!(
                        "no consultation job `{}` to cancel — it may have finished and \
                         been evicted, or never existed.",
                        input.handle
                    ),
                    None,
                ));
            }
        };
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "List your async work: background jobs in flight this session, \
            and the batches the providers still know about (last 24h by default; \
            `all: true` for everything) — each with a ready handle for \
            `job_get`/`job_cancel`. This is the way back to a batch whose handle you \
            lost — the provider's own list is the source of truth."
    )]
    async fn job_list(
        &self,
        Parameters(input): Parameters<ListInput>,
    ) -> Result<CallToolResult, McpError> {
        let mut sections: Vec<String> = Vec::new();

        // In-memory jobs first — this session. `consult_submit` AND `deliberate`'s direct
        // lane both land here, so show the section when either produces jobs.
        if self.config.tools.consult || self.config.tools.deliberate {
            sections.push(render_jobs_section(&self.jobs.list()));
        }

        // Batches — provider-side and durable; `backend` scopes this section only. A
        // batch-resolution failure (no batch-capable backend, or a bad explicit
        // `backend`) becomes a *section note*, not a hard error — so it never sinks the
        // consult-jobs section above it. (A local-only setup with batch on but no
        // hosted backend is the common case here.)
        if self.config.tools.batch || self.config.tools.deliberate {
            match self.batch_backends(input.backend.as_deref()) {
                Ok(backends) => {
                    let mut entries: Vec<(String, crate::batch::BatchListItem)> = Vec::new();
                    let mut errors: Vec<(String, String)> = Vec::new();
                    let mut truncated: Vec<String> = Vec::new();
                    for name in backends {
                        // One keyless or unreachable backend never sinks the whole
                        // listing — turn its failure into a per-backend note (the
                        // per-item-failure ethos, at the backend grain).
                        let listed = match self.batch_poller(&name) {
                            Ok(provider) => {
                                let span = tracing::info_span!("job_list", backend = %name);
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
                    // Trim to the last 24h by default — a provider keeps months of
                    // finished batches, and dumping them all every call just burns the
                    // caller's tokens. The SLA is ≤24h, so anything older is done and
                    // still collectible by its handle; `all: true` shows the full history.
                    // An undateable batch (no/garbled timestamp) is kept, never hidden.
                    let hidden = if input.all {
                        0
                    } else {
                        // Read the clock once, not once per item.
                        let now = now_epoch_secs();
                        let before = entries.len();
                        entries.retain(|(_, it)| {
                            batch_within_window(it, now, BATCH_RECENCY_WINDOW_SECS)
                        });
                        before - entries.len()
                    };
                    sections.push(crate::batch::render_list(&entries, &errors, &truncated));
                    if hidden > 0 {
                        sections.push(format!(
                            "({hidden} batch(es) older than 24h hidden — `job_list` with \
                             `all: true` to see the full history.)"
                        ));
                    }
                }
                Err(e) => sections.push(format!("Batches: unavailable — {}", e.message)),
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            sections.join("\n\n"),
        )]))
    }

    #[tool(description = "Park for async work to make progress: blocks up to \
            `timeout_secs` and returns as soon as a job finishes or fails, or on a \
            clean timeout — the productive alternative to polling `job_get`. \
            `level:\"info\"` adds the live narrative (each shell command, sweep, \
            milestone); name batch `handles` to fold their status in.")]
    async fn job_wait(
        &self,
        Parameters(input): Parameters<WaitInput>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        // No silent clamp — a model picks its own block; only an absurd value is refused,
        // loudly. The client's tool-call timeout and the user's interrupt are the real
        // ceilings (see `WaitInput::timeout_secs`).
        if let Some(t) = input.timeout_secs {
            if t > 3600 {
                return Err(McpError::invalid_params(
                    format!(
                        "timeout_secs {t} is over 3600 (1h) — pass a smaller value, or \
                         call `job_wait` again each time it returns; a single block is \
                         capped by your client's tool-call timeout anyway."
                    ),
                    None,
                ));
            }
        }
        let return_floor = wait_level_floor(input.level.as_deref())?;
        let limit = input.limit.unwrap_or(20);
        let timeout = std::time::Duration::from_secs(input.timeout_secs.unwrap_or(60));

        // The live view for the *human*: while this call is open, stream the Info-level
        // narrative (each kaish command, sweep, milestone) as `notifications/progress` on
        // this call's token, so the client renders it in real time — the channel sync
        // `consult` used, reopened on demand. Independent of what we *return* to the model
        // (Warn+ by default): the human watches the show, the model gets the salient bits.
        let info_floor = crate::mcp_log::rank(LoggingLevel::Info);
        let token = progress_token(&meta);
        // Drain down to whichever is lower — Info (to stream) when a token is present,
        // else just the return floor (don't consume narrative no one is watching).
        let drain_floor = if token.is_some() {
            info_floor.min(return_floor)
        } else {
            return_floor
        };
        let seq = AtomicU64::new(0);
        let records = self
            .notifications
            .wait_drain_with(timeout, drain_floor, return_floor, limit, |rec| {
                // Stream Info+ to the human's progress channel; the model's return is the
                // separate `return_floor` collection inside `wait_drain_with`.
                if let Some(token) = &token {
                    if crate::mcp_log::rank(rec.level) >= info_floor {
                        let param = ProgressNotificationParam {
                            progress_token: token.clone(),
                            progress: seq.fetch_add(1, Ordering::Relaxed) as f64,
                            total: None,
                            message: Some(format!(
                                "[{}] {}",
                                wait_level_label(rec.level),
                                rec.message
                            )),
                        };
                        let peer = peer.clone();
                        // Fire-and-forget, like `ProgressReporter`: don't make the drain
                        // loop await a notification it doesn't depend on.
                        tokio::spawn(async move {
                            let _ = peer.notify_progress(param).await;
                        });
                    }
                }
            })
            .await;

        // Gentle batch poll: a batch is provider-side with no push, so fold in a one-shot
        // status for any batch handle named. Non-batch handles are ignored here (consult
        // jobs surface through the stream + the running-jobs footer).
        let mut batch_lines = Vec::new();
        for h in &input.handles {
            if !is_batch_handle(h) {
                continue;
            }
            // Respect batch gating, like `job_get`/`job_cancel` — don't poll a batch
            // handle on a server that has batch turned off. A per-handle note, not a
            // hard error: it never sinks the rest of the `job_wait`.
            if let Err(e) = self.ensure_batch_enabled(h) {
                batch_lines.push(format!("{h} — {}", e.message));
                continue;
            }
            let line = match parse_batch_handle(h) {
                Ok((backend, id)) => match self.batch_poller(backend) {
                    Ok(provider) => match provider.poll(id).await {
                        Ok(poll) => format!("{h} — {}", batch_poll_brief(&poll)),
                        Err(e) => format!("{h} — poll failed: {e:#}"),
                    },
                    Err(e) => format!("{h} — {}", e.message),
                },
                Err(e) => format!("{h} — {}", e.message),
            };
            batch_lines.push(line);
        }

        Ok(CallToolResult::success(vec![Content::text(render_wait(
            &records,
            &batch_lines,
            &self.jobs,
            timeout,
        ))]))
    }

    /// Refuse a batch-shaped handle only when *no tool that produces one* is enabled —
    /// `job_get`/`job_cancel` survive as long as any producer is on, so a handle can
    /// arrive for a producer that's off. A `backend/id` handle comes from `batch_submit`
    /// OR `deliberate`'s batch lane, so either capability keeps it collectible.
    fn ensure_batch_enabled(&self, handle: &str) -> Result<(), McpError> {
        if self.config.tools.batch || self.config.tools.deliberate {
            return Ok(());
        }
        Err(McpError::invalid_params(
            format!(
                "`{handle}` looks like a batch handle (`backend/id`), but nothing that \
                 produces one is enabled on this server (--no-batch --no-deliberate)."
            ),
            None,
        ))
    }

    /// Refuse a `job-N` handle only when no tool that produces one is enabled. A `job-N`
    /// comes from `consult_submit` OR `deliberate`'s direct lane, so either keeps it
    /// collectible.
    fn ensure_consult_enabled(&self, handle: &str) -> Result<(), McpError> {
        if self.config.tools.consult || self.config.tools.deliberate {
            return Ok(());
        }
        Err(McpError::invalid_params(
            format!(
                "`{handle}` looks like a background job (`job-N`), but nothing that \
                 produces one is enabled on this server (--no-consult --no-deliberate)."
            ),
            None,
        ))
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
        markdown_resource(
            TOOLS_URI,
            "kaibo: using the tools",
            "How to wield kaibo's tools well: attachments, picking a cast/model, the \
             sync↔async pairs and their handles, and read-only-shell idioms. The tool \
             schemas stay terse and point here.",
        ),
        markdown_resource(
            PROMPTS_URI,
            "kaibo: the prompts models get",
            "The exact system preamble each phase gets (explorer, consult, oneshot, \
             batch/deliberate synth), rendered by the same code the tools run. \
             kaibo://prompts/<cast> shows one cast's resolved framing.",
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
Anthropic / DeepSeek / Gemini / OpenRouter I hold API keys for, and whether I run any \
OpenAI-compatible local servers (llama.cpp, Ollama, an image server) and at what base \
URLs. Let me tell you my providers rather than guessing them. OpenRouter is worth \
naming on its own — one key there reaches every major model family through a single \
gateway.
3. Propose a roster built on a provider I actually named in step 2, then write it to \
`$XDG_CONFIG_HOME/kaibo/config.toml` (default `~/.config/kaibo/config.toml`). The \
default shape is a single outside family — DeepSeek, Gemini, Anthropic, OpenRouter, or \
a local pair — with explorer and synth both within it. That one family is already the \
whole win: it augments my own lineage with a different house's eyes (a cheap, fast \
explorer and a stronger synth, same family). kaibo's built-in casts are already \
within-family pairs, so often this is just giving one of them a key rather than writing \
a new cast. Mixing families across roles (a 'chimera' — say a DeepSeek explorer with a \
Claude synth) is an advanced move for someone who holds several keys and asks for it; \
don't reach for it by default. If OpenRouter is the family, ground the model picks in \
its live catalog instead of guessing ids: `GET https://openrouter.ai/api/v1/models` is \
public, no auth, and filters to what matters — \
`?supported_parameters=tools&category=programming&sort=intelligence-high-to-low` finds \
tool-capable coding models (a consult cast needs `tools` support); `q=` / `context=` / \
`max_price=` narrow further; each entry carries live pricing, context length, and a \
`reasoning` capability block. Favor the drift-proof `~author/family-latest` aliases \
(e.g. `~anthropic/claude-sonnet-latest`) over a pinned slug, and know that `:free` / \
`:nitro` / `:floor` suffixes pick a free, fastest, or cheapest variant of a concrete \
slug where offered.
4. Keep secrets in the environment or a key file. A backend names an env var \
(`api_key_env`) or a key-file path (`api_key_file`); the TOML carries the name or path, \
the secret stays outside it. Tell me which env vars to set or files to write, and let \
me put the keys in myself.
5. (Optional) Read scope. By default kaibo reads only the project tree (plus linked git \
worktrees) and only ever *reads* — never writes. To let the team see a scratch space — a \
diff, a log, a generated file you dropped somewhere — name that directory in \
`[server] allow_paths` (`$VAR` / `${VAR}` and a leading `~` expand, resolving per machine). \
It's a deliberate opt-in worth asking me about first, since it widens what a consult can \
read (and can ship to a model).
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

/// The URI templates kaibo advertises: per-builtin help and per-cast prompts, each
/// addressed by name.
fn kaibo_resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
    let builtin = RawResourceTemplate {
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
    let prompts = RawResourceTemplate {
        uri_template: PROMPTS_CAST_URI_TEMPLATE.to_string(),
        name: "kaibo: one cast's prompts".to_string(),
        title: None,
        description: Some(
            "The system preamble each phase gets for a specific cast, its per-slot \
             `preamble`s folded in as a live call resolves them. e.g. kaibo://prompts/deepseek"
                .to_string(),
        ),
        mime_type: Some("text/markdown".to_string()),
        icons: None,
    };
    vec![builtin.no_annotation(), prompts.no_annotation()]
}

/// Render the markdown body for a kaibo resource URI, or `None` if the URI isn't
/// one kaibo serves. Pure and offline-testable; the handler wraps the result.
/// The body of the `kaibo://tools` resource. Written generously and positively (the
/// AGENTS.md house style): name the good idiom, say the high-value things a couple of
/// ways, and reserve the few "no"s for habits a calling model carries in from `bash`
/// that genuinely won't work here. This is the long-form home for guidance the tool
/// schemas only gesture at, so it can afford the repetition the schemas can't.
const TOOLS_DOC: &str = "\
# Using kaibo's tools

kaibo lends your work a second opinion from models *outside your own family*, and a
read-only window into a codebase. The tool schemas stay terse on purpose; this is the
longer guide to wielding them well. Read it once and you'll pick the right tool and the
right arguments by feel.

## Hand a model files without pasting them: `attach`

Every path you `attach` is read by kaibo, under its read-only boundary — the bytes never
pass back through *your* context. That's the whole point: keep your context lean, let
kaibo carry the files. One semantic everywhere — **the answering model sees the bytes** —
delivered per tool:

- **`consult` / `consult_submit` — inlined, and pushed to the sweeps.** Text attachments
  splice into the investigation prompt whole, lines numbered like `cat -n`, so the model
  cites them by exact `file:line` (files past the inline budget — `[defaults]
  inline_attach_budget`, default 256 KiB — are instead ordered read WHOLE through the
  model's shell, never silently dropped). Every delegated explorer sweep is also directed
  to read them whole, so a sub-agent is never blind to the files you flagged. Hand it the
  files a question centers on: `attach: [\"src/server.rs\", \"docs/architecture.png\"]`.
  An attached image opens via `view_image` and needs a vision-capable cast — kaibo
  refuses an image to a blind synth up front rather than name a file it could never open.
- **`explore` — read-whole directives.** Its investigator reads through the shell, so
  attached text files become orders to read each one whole during the sweep. Text only;
  attach images to `consult` with a vision cast.
- **`oneshot` / `batch_submit` — inlined.** These models are tool-less — they can't go
  read the repo — so kaibo splices the file bytes straight into the prompt (numbered the
  same way). Give them the *whole* file(s): `[\"README.md\", \"src/server.rs\"]`, not a
  snippet. Top-tier models carry very large context windows (1M+ tokens), so be generous —
  attach whole files, several if they're relevant, rather than trimming. The model has
  room to work; let it see the full picture. Text files splice in as text; images
  (png/jpeg/gif/webp) ride as native image parts and want a vision-capable model
  (`kaibo://config` shows each slot's `vision`).

Prefer whole files to excerpts, and a prose summary of *intent* to a raw paste — your
intent is the part kaibo can't recover from the source itself. **Reviewing a change?**
Lead with the whole files it touched and describe what you did; the answering models tend
to review better from the full files than from a diff alone. A diff can ride along to
point at the moved lines (`git diff > changes.diff` under the repo, then attach it), but
prefer the files — the diff is a pointer, not the context. Paths resolve under kaibo's
allowed set: the project root, plus any linked git worktree kaibo is following — a
sibling-branch checkout next to the repo just works, and `kaibo://config` shows the live
set. A path outside that set, a directory, a missing file, an oversized file, or a binary
that isn't a known image is refused with a clear error — kaibo tells you, it doesn't drop
it silently.

## Pick the team: `cast`, and per-call model overrides

A **cast** is a model team. Omit `cast` for the server's default, or name one — the
`cast` parameter's enum lists the casts live right now, and `kaibo://config` has the full
roster with every backend and alias. Picking a cast from a *different* family than the one
you're running is the whole value: a fresh set of eyes on your work.

For a one-off without editing config, override the model on the call itself:

- `consult` / `consult_submit`: `explorer_model` (+ `explorer_backend`) and/or
  `synth_model` (+ `synth_backend`).
- `oneshot` / `batch_submit`: `model` (+ `backend`).

A model id is sent **verbatim** — an id with a `/` in it (HuggingFace-style
`org/model-name`) is still one id, not a path. The `*_model` override keeps the slot's
configured backend; pair it with the matching `*_backend` (`synth_backend`, `backend`, …)
to retarget the slot to a different connection wholesale — which also lets you fill a role
the cast doesn't otherwise carry.

## Survey the code, or get an answer: `explore` vs `consult`

`consult` hands back an *answer* — a capable model investigates and concludes. `explore`
hands back the *evidence*: it's the fast, cheap explorer half of `consult`, run on its
own, so you get the structured cited report — a summary of findings, the relevant
`file:line` locations, and the trail the explorer followed — with no synthesis on top.
Reach for `explore` to map unfamiliar code, or to assemble a grounded survey you'll reason
over yourself (or hand to another model). It reads the repo itself, like `consult`, so the
same `path` / `cast` / `explorer_model` / `explorer_backend` arguments apply, plus `attach`
(text files the investigator is ordered to read whole during the sweep); no `context` or
`session_id` — those belong to the synthesizing tools. Since it runs
*only* the explorer, its `cast` accepts any cast with an explorer — including `deliberate`/
`direct` casts: point it at one to run that team's (often smarter, slower) explorer
standalone, when you want a stronger sweep than your own fast one, or to size the explorer up.
When you want the conclusion rather than the map, use `consult`.

## Deepest reasoning, offline: `deliberate`

`deliberate` is `explore → offline synth`: a fast model builds a cited dossier (you wait
for this — the same live explorer sweep `explore` runs, minutes), then a heavyweight synth
reasons over that evidence *offline*, so you don't hold a session open for the slow part.
Reach for it on a hard question worth the wait — a design review, a gnarly bug, \"is this
abstraction right\".

The synth's **lane** (a per-slot property of the cast) picks the mechanism and the handle:

- **`batch`** — a frontier model on the provider's batch lane (max thinking, half price).
  `deliberate` returns a durable `backend/provider-id` handle the moment the dossier is
  submitted; collect the deliberation with `job_get` any time, even after a restart.
- **`direct`** — one long completion on a big *local* model (no batch API; it takes the
  time it takes). Returns a session-scoped `job-N`; `job_wait`/`job_get` it. Session-scoped
  means a server restart loses it — collect it in the same session.

A deliberate cast pairs an interactive explorer with an offline synth (e.g. the example
config's `fable`, `gemini-deliberate`, or `local-direct`); `kaibo://config` shows each
cast's lane. Because the synth is toolless and can't come back for more, the dossier is
built whole up front — deliberate reads the repo itself, so it takes `path` / `cast` /
`explorer_model` / `synth_model` (+ their `*_backend`s) and `attach` (text files the
dossier-builder is ordered to read whole, so their content reaches the offline synth
through the dossier), but no `context` / `session_id`. For an answer this turn, use
`consult`.

## Answer now, or hand off and collect later

Each investigation/answer tool comes in a synchronous form and an async sibling. Use the
sync form when you want the answer in this turn; reach for the async form to run several
at once, or when a deep job would otherwise block you.

- **`consult` → `consult_submit`.** Same investigation, but submit hands back a job
  handle and runs in the background. Great for a cross-model study: submit one per cast,
  go do other work, collect them all.
- **`oneshot` → `batch_submit`.** Same toolless answer, but batch fans many prompts onto
  the provider's cheaper offline lane at max thinking, and hands back a handle.

**Handles tell you their kind by shape**, and you pass back whatever you were given:

- A **consult** handle is `job-N` (e.g. `job-1`). It's in-memory — it lives for *this*
  server session only, so collect it before you reconnect.
- A **batch** handle is `backend/provider-id` (e.g. `anthropic/msgbatch_…`). It's durable
  — it survives a server restart, so you can always come back for it.

One small surface drives both kinds:

- **`job_get <handle>`** collects a job — a progress line while it works, the full answer
  once it's done (batch: every item's answer, labelled by index, per-item failures
  surfaced).
- **`job_cancel <handle>`** stops a running job. A job that already finished is left alone.
- **`job_list`** shows everything: consult jobs in flight this session, and the batches
  the providers still know about — the way back to a handle you've lost. By default the
  batches section shows the last 24h (anything older is done and still collectible by its
  handle); pass `all: true` for the full history.
- **`job_wait`** is how you *productively park*: submit your async work, do your other
  work, then call `job_wait` to block briefly (up to `timeout_secs` — your choice, to a
  3600s ceiling) and return as soon
  as something lands — or on a clean timeout. By default it hands back what kaibo flagged
  for you (a job finished or failed); pass `level: \"info\"` to also pull in the watchable
  narrative — each kaish command, sweep, and milestone the agents ran.

This is a fire-and-forget lane. Submit, then go do other work — don't sit in a tight
poll/sleep loop holding your turn open. `job_wait` when you're ready to spend a minute;
`job_get`/`job_list` are the source of truth. Nothing here wakes you, and nothing is lost
by waiting — the handles keep.

## Driving the read-only shell (`run_kaish`)

`run_kaish` runs a kaish (sh-like) script against the project and returns exit code +
stdout + stderr. Lead with the idioms that produce accurate `file:line`s: `cat -n FILE`
to read a file WHOLE (the default move — nearly every file fits one look),
`grep -rn PATTERN .` to find which files matter (alternation takes `-E`:
`grep -rnE 'foo|bar' .`). A whole read that truncates (exit 3) hands back the head and
tail; stage the rest as targeted wide spans (`grep -n SYMBOL FILE`, then
`cat -n FILE | sed -n '1200,2400p'`). Compose builtins with pipes
(`grep`/`jq`/`awk`/`find`/…). Each call starts fresh at the project root.

A few habits from `bash` that *won't* carry over here — reach for the kaish form instead:

- `$VAR` is **one word**, always — kaish never splits it on whitespace. When you actually
  want to split, use the `split` builtin; that's the deliberate form.
- Adjacent tokens don't paste together — quote to join: `\"$dir/file.txt\"`, not
  `$dir/file.txt`.
- This shell is **read-only**: a write, a redirect that would create a file, or an
  external command is refused (exit 126 = blocked by the sandbox; 124 = killed for running
  too long). That's the boundary working, not a bug — read freely, and don't try to mutate.

Learn more without spending a turn: the `kaibo://kaish/*` resources (syntax, builtins,
vfs, scatter, …) and `kaibo://kaish/sandbox`, or run `help` / `help syntax` /
`help <builtin>` right in the script.

## Seeing (and tuning) the prompts the models get

`kaibo://prompts` shows the exact system preamble each phase receives — the explorer
sweep, the `consult` driver, `oneshot`, and the offline `batch`/`deliberate` synth —
rendered by the same code a live call runs, with any `[prompts]` override folded in. It
also shows how the question is wrapped into the user turn. Read it to audit what a model
is actually told, or before you tune a preamble: override a phase's role framing globally
with the `[prompts]` table, or per cast with a slot's `preamble` (the two axes and the
`[orientation]`/`[context]` layers are laid out there and in `kaibo://config`).
";

fn render_resource(uri: &str, schemas: &[ToolSchema]) -> Option<String> {
    if uri == SANDBOX_URI {
        return Some(kaibo_sandbox_doc());
    }
    if uri == TOOLS_URI {
        return Some(TOOLS_DOC.to_string());
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

/// Render the `kaibo://prompts` document — or `kaibo://prompts/{cast}` when `cast` is
/// `Some`. Each phase's system preamble is produced by the *same*
/// [`resolve_phase_preamble`](crate::consult::resolve_phase_preamble) the live tools call,
/// so the text can't drift from a real call; the user-turn framing likewise renders through
/// the real [`consult_user_prompt`](crate::consult::consult_user_prompt) /
/// [`deliberation_prompt`](crate::consult::deliberation_prompt).
///
/// `cast = None` is the cast-independent view: built-in framing or a global `[prompts]`
/// override. `cast = Some` folds in that cast's per-slot `preamble`s via
/// [`Cast::resolved_prompts`] — the same layering a call runs — and attributes each phase
/// to the slot that framed it. Either way the two *path*-dependent layers (`[orientation]`
/// map, `[context]` house rules) are named, not rendered: they resolve per call against a
/// path a static resource lacks.
fn render_prompts_resource(config: &Config, cast: Option<&Cast>) -> String {
    use crate::consult::{consult_user_prompt, deliberation_prompt, resolve_phase_preamble, Phase};

    // The role-framing layer to render: a cast folds its per-slot preambles over the
    // global table (exactly a live call's resolution); no cast shows the global table.
    let prompts = match cast {
        Some(c) => c.resolved_prompts(&config.prompts),
        None => config.prompts.clone(),
    };

    let mut out = String::new();
    match cast {
        Some(c) => out.push_str(&format!(
            "# The prompts cast `{}` gets\n\n\
             The system preamble each phase receives for this cast, its per-slot \
             `preamble`s folded in — rendered by the same code a live call runs. The \
             `[orientation]` map and `[context]` house rules still append per call \
             (project-reading phases, path-dependent). `kaibo://prompts` is the \
             cast-independent view (and shows the user-turn framing).\n",
            c.name
        )),
        None => out.push_str(
            "# The prompts kaibo's models get\n\n\
             The system preamble each phase receives, rendered by the same code a live call \
             runs — so this is what the model reads, not a paraphrase. Cast-independent \
             view: the built-in framing, or a global `[prompts]` override. For one cast's \
             resolved framing (its per-slot `preamble`s folded in) read \
             `kaibo://prompts/<cast>`. The `[orientation]` map and `[context]` house rules \
             append per call for the project-reading phases (path-dependent).\n\n\
             A phase is a role, not one tool — several tools share a preamble. The \
             **explorer** framing drives standalone `explore`, the delegated sweep inside \
             `consult`, and `deliberate`'s dossier-building pass; the **offline-synth** \
             framing serves both `batch_submit` and `deliberate`'s synth. So tuning one \
             phase moves every tool that wears it.\n",
        ),
    }

    for phase in Phase::ALL {
        // Attribute the framing by precedence: a cast slot's `preamble` wins over a global
        // `[prompts]` override wins over the built-in — the order `resolved_prompts` layers.
        let slot_set = cast
            .and_then(|c| c.slot(phase.slot_role()))
            .and_then(|s| s.preamble.as_deref())
            .is_some();
        let tag = if slot_set {
            format!(
                "cast `{}` slot `preamble`",
                cast.expect("slot_set ⇒ cast").name
            )
        } else if phase.override_in(&config.prompts).is_some() {
            "global `[prompts]` override".to_string()
        } else {
            "kaibo built-in".to_string()
        };
        let project = if phase.reads_project() {
            "Reads the project → the `[orientation]` map and `[context]` house rules append per call."
        } else {
            "Owns its context (the caller supplies it) → no project layers."
        };
        // The one seam the tools use. `None, None` for the path layers: this static doc
        // can't resolve a path, and we've named that above.
        let body = resolve_phase_preamble(phase, &prompts, None, None);
        out.push_str(&format!(
            "\n---\n\n## {}\n\n_{}_ · {}\n\n```text\n{}\n```\n",
            phase.label(),
            tag,
            project,
            body
        ));
    }

    // The user-turn framing is cast-independent, so it lives on the base doc only; the
    // per-cast view points back to it rather than repeating it.
    if cast.is_none() {
        out.push_str(
            "\n---\n\n## User-turn framing\n\n\
             The system preamble sets the role; the **user turn** carries your question. \
             kaibo wraps it — the renders below use placeholder inputs, but the wrapping is \
             the real code:\n\n\
             ### `consult` / `consult_submit`\n\n\
             With a `context` (a session history and `attach`ed files add further blocks; a \
             bare question with none of these is sent verbatim):\n\n```text\n",
        );
        out.push_str(&consult_user_prompt(
            "<your question>",
            Some("<a diff or change summary, a prior report, or pasted source>"),
            &[],
            &[],
        ));
        out.push_str(
            "\n```\n\n### `deliberate` (offline synth over the explorer's dossier)\n\n```text\n",
        );
        out.push_str(&deliberation_prompt(
            "<your question>",
            "<the explorer's cited dossier — SummaryOfFindings, RelevantLocations with \
             file:line snippets, ExplorationTrace>",
        ));
        out.push_str("\n```\n");
    }
    out
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
    if uri == PROMPTS_URI {
        return Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(
                render_prompts_resource(config, None),
                uri,
            )],
        });
    }
    if let Some(name) = uri.strip_prefix(PROMPTS_CAST_PREFIX) {
        // `kaibo://prompts/<cast>` — the cast's resolved framing (name or alias). An
        // unknown cast is a not-found whose message already names the known casts, so a
        // caller sees the real roster, not a bare miss.
        let cast = config
            .resolve_cast(name)
            .map_err(|e| McpError::resource_not_found(format!("{e:#} (in {uri})"), None))?;
        return Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(
                render_prompts_resource(config, Some(cast)),
                uri,
            )],
        });
    }
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
    use serde_json::json;

    /// deliberate-direct's wall-clock backstop tracks its synth backend's own
    /// `request_timeout` (+ margin), NOT the interactive `call_deadline`. This is the
    /// decision that keeps a slow local `deliberate` from forcing the interactive
    /// ceiling high: give that model 3h of `request_timeout` and the direct job inherits
    /// it, while `consult`/`explore`/`oneshot` stay bounded at the tight `call_deadline`.
    #[test]
    fn deliberate_direct_deadline_tracks_request_timeout_not_call_deadline() {
        let cfg = crate::config::Config::builtin();
        let mut backend = cfg
            .resolve_backend("openai-local")
            .expect("built-in openai backend")
            .clone();
        backend.request_timeout = std::time::Duration::from_secs(3 * 60 * 60); // a slow local model, 3h of patience
        let deadline = deliberate_direct_deadline(&backend);
        assert_eq!(
            deadline,
            std::time::Duration::from_secs(3 * 60 * 60) + DELIBERATE_DEADLINE_MARGIN,
            "the direct-lane backstop is the synth request_timeout + margin"
        );
        assert!(
            deadline > cfg.defaults.call_deadline,
            "a 3h-patience local synth must outlast the interactive ceiling ({:?}), not be capped by it",
            cfg.defaults.call_deadline
        );
    }

    /// consult `attach` validates files are under the consult root (so the model's shell
    /// can `cat` them) and returns them as relative paths; a file outside the root — even
    /// a real, readable one — is refused, since the shell couldn't reach it. The root here
    /// stands in for any tree, including a followed worktree (which `resolve_root` returns
    /// as the root verbatim).
    #[tokio::test]
    async fn consult_attach_keeps_under_root_relative_and_rejects_outside() {
        let root = tempfile::tempdir().unwrap();
        let root_canon = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir(root_canon.join("src")).unwrap();
        std::fs::write(root_canon.join("src/jobs.rs"), b"// in tree").unwrap();

        // A relative path resolves to its root-relative form (what the model `cat`s).
        let rel = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &["src/jobs.rs".to_string()],
            1 << 18,
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect("an in-tree file resolves");
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].path(), "src/jobs.rs");

        // A file outside the root is refused, even though it exists and is readable.
        let outside = tempfile::tempdir().unwrap();
        let outside_file = std::fs::canonicalize(outside.path())
            .unwrap()
            .join("x.diff");
        std::fs::write(&outside_file, b"diff").unwrap();
        let err = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &[outside_file.display().to_string()],
            1 << 18,
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect_err("an out-of-root file must be refused");
        assert!(
            err.message.contains("outside the project root"),
            "the refusal names the boundary: {}",
            err.message
        );
    }

    /// consult `attach` sniffs each file's *content* (not its extension) so the driver
    /// prompt routes it right: a text file inlines (or demotes to a shell read), an image
    /// goes to `view_image`. A PNG signature classifies as an image even named `.txt`, and
    /// a UTF-8 file classifies as text even named `.png` — content is the ground truth,
    /// matching how `view_image` re-sniffs at read time.
    #[tokio::test]
    async fn consult_attach_classifies_images_by_content_not_extension() {
        let root = tempfile::tempdir().unwrap();
        let root_canon = std::fs::canonicalize(root.path()).unwrap();
        // A real PNG magic number, deliberately misnamed `.txt`.
        let png_sig = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(root_canon.join("shot.txt"), png_sig).unwrap();
        // UTF-8 source, deliberately misnamed `.png`.
        std::fs::write(root_canon.join("notes.png"), b"// just text").unwrap();

        let out = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &["shot.txt".to_string(), "notes.png".to_string()],
            1 << 18,
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect("both resolve");
        let by_path = |p: &str| out.iter().find(|a| a.path() == p).unwrap().clone();
        assert!(
            by_path("shot.txt").is_image(),
            "PNG bytes classify as image despite .txt"
        );
        match by_path("notes.png") {
            crate::consult::ConsultAttachment::Text { body, .. } => {
                assert_eq!(
                    body, "// just text",
                    "UTF-8 bytes inline as text despite .png"
                )
            }
            other => panic!("UTF-8 file must inline as Text, got {other:?}"),
        }
    }

    /// The inline budget is cumulative in caller order: files inline until one doesn't
    /// fit, which demotes (named + size) while a later smaller file may still inline.
    /// Budget 0 — the small-context escape hatch — inlines nothing and demotes every
    /// text file; nothing is ever silently dropped.
    #[tokio::test]
    async fn consult_attach_inlines_within_budget_and_demotes_past_it() {
        let root = tempfile::tempdir().unwrap();
        let root_canon = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(root_canon.join("small.rs"), b"fn a() {}").unwrap(); // 9 bytes
        std::fs::write(root_canon.join("big.rs"), vec![b'x'; 64]).unwrap(); // 64 bytes
        std::fs::write(root_canon.join("tiny.rs"), b"ok").unwrap(); // 2 bytes
        let paths = vec![
            "small.rs".to_string(),
            "big.rs".to_string(),
            "tiny.rs".to_string(),
        ];

        // Budget 16: small (9) inlines, big (64) demotes, tiny (2) still fits after.
        let out = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &paths,
            16,
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect("all resolve");
        assert!(
            matches!(&out[0], crate::consult::ConsultAttachment::Text { body, .. } if body == "fn a() {}"),
            "under-budget file inlines: {out:?}"
        );
        assert!(
            matches!(
                &out[1],
                crate::consult::ConsultAttachment::TextOversize { size: 64, .. }
            ),
            "over-budget file demotes with its size: {out:?}"
        );
        assert!(
            matches!(&out[2], crate::consult::ConsultAttachment::Text { body, .. } if body == "ok"),
            "a later small file still fits the remaining budget: {out:?}"
        );

        // Budget 0: everything demotes — the instruct-only escape hatch.
        let out = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &paths,
            0,
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect("all resolve");
        assert_eq!(out.len(), 3, "nothing is dropped");
        assert!(
            out.iter()
                .all(|a| matches!(a, crate::consult::ConsultAttachment::TextOversize { .. })),
            "budget 0 demotes every text file: {out:?}"
        );
    }

    /// The whole pipeline, raw paths to prompt: real files in a tempdir go through
    /// `resolve_consult_attachments` (prefix sniff, VFS read, budget partition,
    /// root-relative labeling) and the result feeds `consult_user_prompt` — asserting on
    /// the final text a driver model would actually see. Catches seam mismatches the
    /// per-piece tests can't (e.g. a resolver label the prompt renderer mangles).
    #[tokio::test]
    async fn consult_attach_pipeline_resolves_and_renders_end_to_end() {
        let root = tempfile::tempdir().unwrap();
        let root_canon = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir(root_canon.join("src")).unwrap();
        std::fs::write(root_canon.join("src/small.rs"), b"fn a() {}\nfn b() {}").unwrap();
        std::fs::write(root_canon.join("src/big.rs"), vec![b'x'; 128]).unwrap();
        let png_sig = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(root_canon.join("shot.png"), png_sig).unwrap();

        let attachments = KaiboHandler::resolve_consult_attachments(
            &root_canon,
            &[
                "src/small.rs".to_string(),
                "src/big.rs".to_string(),
                "shot.png".to_string(),
            ],
            64, // small.rs (19 B) inlines; big.rs (128 B) demotes
            &crate::sandbox::SandboxConfig::default(),
        )
        .await
        .expect("all three resolve");
        let prompt =
            crate::consult::consult_user_prompt("Assess it.", None, &[], &attachments);

        assert!(
            prompt.contains("<file path=\"src/small.rs\">"),
            "inlined file labeled root-relative:\n{prompt}"
        );
        assert!(
            prompt.contains("     1\tfn a() {}\n     2\tfn b() {}"),
            "inlined body numbered like cat -n:\n{prompt}"
        );
        assert!(
            prompt.contains("- src/big.rs (128 bytes)"),
            "oversize file demoted with its size:\n{prompt}"
        );
        assert!(
            prompt.contains("Read each one WHOLE"),
            "command-voice directive present:\n{prompt}"
        );
        assert!(
            prompt.contains("view_image") && prompt.contains("- shot.png"),
            "image routed to view_image:\n{prompt}"
        );
        assert!(
            !prompt.contains("xxxx"),
            "demoted bytes never reach the prompt:\n{prompt}"
        );
    }

    /// An image attached to a vision-blind consult synth is refused honestly up front —
    /// consult would have no way to show it (no `view_image` without vision), so naming the
    /// file would be a lie. A vision synth passes; text-only always passes either way.
    #[test]
    fn consult_image_attach_is_gated_on_synth_vision() {
        let img = vec![crate::consult::ConsultAttachment::Image {
            path: "shot.png".to_string(),
        }];
        let txt = vec![crate::consult::ConsultAttachment::Text {
            path: "notes.md".to_string(),
            body: "notes".to_string(),
        }];
        // Blind synth + image → refused, naming the cast and the vision requirement.
        let err = KaiboHandler::gate_consult_image_attachments(
            &img,
            false,
            "deepseek-v4-pro",
            "deepseek",
        )
        .expect_err("an image to a blind synth must be refused");
        assert!(
            err.message.contains("can't see images") && err.message.contains("deepseek"),
            "the refusal names the cause and the cast: {}",
            err.message
        );
        // Vision synth + image → fine; blind synth + text-only → fine.
        KaiboHandler::gate_consult_image_attachments(&img, true, "claude-sonnet-4-6", "anthropic")
            .expect("a vision synth accepts an image");
        KaiboHandler::gate_consult_image_attachments(&txt, false, "deepseek-v4-pro", "deepseek")
            .expect("text-only needs no vision");
    }

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

    /// The joined text of a successful `CallToolResult` — for asserting on a handler's
    /// reply message.
    fn result_text(r: CallToolResult) -> String {
        r.content
            .into_iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A keyless config with both a synth-only batch cast (`mybatch`) and a
    /// deliberate-shaped batch cast (`mydeliberate`, explorer + batch synth). Keyless so
    /// the *real* provider build would need no key anyway; tests inject a scripted factory
    /// so no network is touched regardless.
    const BATCH_CASTS_TOML: &str = r#"
        [backends.gem]
        kind = "gemini"
        key_optional = true

        [casts.mybatch]
        batch = true
        synth = "gem/some-pro"

        [casts.mydeliberate]
        explorer = "gem/some-lite"
        synth    = { backend = "gem", id = "some-pro", lane = "batch" }
    "#;

    /// The live cast roster is stamped onto each consultation tool's `cast` param as
    /// a JSON-Schema `enum`, so an agent reads the menu off the schema it fills
    /// arguments from — the fix for casts being discoverable only in handshake prose
    /// a host may truncate. The keyless local `openai-local` cast is always usable, so it
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
                variants.iter().any(|v| v == "openai-local"),
                "{tool}: cast enum should list the always-usable local cast, got {variants:?}"
            );
        }
    }

    /// `consult` is the front door: pinning it `anthropic/alwaysLoad` means the calling
    /// model sees its description even when the host defers tool schemas to names-only,
    /// with no extra lookup round-trip. `oneshot` is the negative control — it must NOT
    /// carry the pin, proving the meta is targeted at `consult` alone, not stamped
    /// server-wide.
    #[test]
    fn consult_is_pinned_always_load() {
        let h = KaiboHandler::new(Config::builtin()).expect("handler builds");
        let consult_meta = h
            .tool_router
            .get("consult")
            .expect("consult advertised")
            .meta
            .clone()
            .expect("consult must carry _meta");
        assert_eq!(
            consult_meta.get("anthropic/alwaysLoad"),
            Some(&serde_json::Value::Bool(true)),
            "consult must be pinned resident under schema deferral, got {consult_meta:?}"
        );

        let oneshot_meta = h
            .tool_router
            .get("oneshot")
            .expect("oneshot advertised")
            .meta
            .clone();
        let oneshot_pinned = oneshot_meta
            .as_ref()
            .and_then(|m| m.get("anthropic/alwaysLoad"))
            == Some(&serde_json::Value::Bool(true));
        assert!(
            !oneshot_pinned,
            "only consult should be pinned, but oneshot carries the pin too: {oneshot_meta:?}"
        );
    }

    /// The cast roster splits by lane AND by whether a cast carries an explorer, across
    /// the advertised `cast` enums: interactive tools list non-offline casts;
    /// `batch_submit` lists batch synths (explorer or not); `deliberate` lists offline
    /// casts that ALSO have an explorer (its dossier phase). So a deliberate-shaped batch
    /// cast (`mydeliberate`) rides both batch and deliberate; a synth-only batch cast
    /// (`mybatch`) rides batch only; a synth-only `direct` cast (`mydirect`) rides none
    /// (no explorer → nothing to build its dossier). Driven through a keyless local gemini
    /// backend so every cast is usable regardless of the test env's API keys.
    #[test]
    fn cast_enums_split_by_lane() {
        let h = handler_from_toml(
            r#"
            # A keyless (placeholder) batch-capable backend so all casts are
            # "usable" offline — the partition is exercised with teeth, not trivially empty.
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.mybatch]
            batch = true
            synth = "gem/some-pro"

            [casts.myinteractive]
            explorer = "gem/some-lite"
            synth = "gem/some-flash"

            [casts.mydirect]
            synth = { backend = "gem", id = "some-big-local", lane = "direct" }

            [casts.mydeliberate]
            explorer = "gem/some-lite"
            synth = { backend = "gem", id = "some-pro", lane = "batch" }
            "#,
        );
        let enum_of = |tool: &str| -> Vec<String> {
            h.tool_router
                .get(tool)
                .expect("tool advertised")
                .input_schema
                .get("properties")
                .and_then(|p| p.get("cast"))
                .and_then(|c| c.get("enum"))
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        // The interactive tools need an interactive synth: only `myinteractive`.
        for tool in ["consult", "consult_submit", "oneshot"] {
            let casts = enum_of(tool);
            assert!(
                casts.iter().any(|c| c == "myinteractive"),
                "{tool} enum should list the interactive cast, got {casts:?}"
            );
            for offline in ["mybatch", "mydirect", "mydeliberate"] {
                assert!(
                    !casts.iter().any(|c| c == offline),
                    "{tool} enum must not list the offline cast {offline}, got {casts:?}"
                );
            }
        }
        // `explore` runs only the explorer, so it advertises *every* cast with one —
        // interactive AND offline-synth casts (`mydeliberate`) — but not the synth-only
        // casts (`mybatch`, `mydirect`), which have no explorer to run.
        let explore = enum_of("explore");
        for with_explorer in ["myinteractive", "mydeliberate"] {
            assert!(
                explore.iter().any(|c| c == with_explorer),
                "explore enum should list the explorer-bearing cast {with_explorer}, got {explore:?}"
            );
        }
        for no_explorer in ["mybatch", "mydirect"] {
            assert!(
                !explore.iter().any(|c| c == no_explorer),
                "explore enum must not list the synth-only cast {no_explorer} (no explorer), \
                 got {explore:?}"
            );
        }
        let batch = enum_of("batch_submit");
        // Both batch synths (explorer or not) can be batch_submit'd — it's synth-only.
        assert!(
            batch.iter().any(|c| c == "mybatch") && batch.iter().any(|c| c == "mydeliberate"),
            "batch_submit enum should list both batch synths, got {batch:?}"
        );
        for not_batch in ["myinteractive", "mydirect"] {
            assert!(
                !batch.iter().any(|c| c == not_batch),
                "batch_submit enum must not list {not_batch}, got {batch:?}"
            );
        }
        let deliberate = enum_of("deliberate");
        // Only the offline-synth-WITH-explorer cast staffs a deliberation.
        assert!(
            deliberate.iter().any(|c| c == "mydeliberate"),
            "deliberate enum should list the explorer+offline-synth cast, got {deliberate:?}"
        );
        for not_deliberate in ["mybatch", "mydirect", "myinteractive"] {
            assert!(
                !deliberate.iter().any(|c| c == not_deliberate),
                "deliberate enum must not list {not_deliberate} (no explorer, or interactive \
                 synth), got {deliberate:?}"
            );
        }
    }

    /// The anti-drift guard for the lane partition: whatever `CAST_ENUM_RULES` advertises
    /// on a tool's `cast` enum, that tool's call-time GATE must accept — so the menu the
    /// model picks from never offers a cast the handler would refuse. Reads the *shipped*
    /// enum (not the rules table) and runs each advertised cast through the real gate, over
    /// a fixture with every cast shape. If a future edit points a tool's enum at the wrong
    /// predicate, or a gate tightens without the enum following, this fails.
    #[test]
    fn cast_enum_never_advertises_a_gated_cast() {
        let h = handler_from_toml(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.inter]                                      # explorer + interactive synth
            explorer = "gem/lite"
            synth    = "gem/flash"

            [casts.oneshot_only]                               # synth-only, interactive
            synth    = "gem/flash"

            [casts.mybatch]                                    # synth-only batch
            batch    = true
            synth    = "gem/pro"

            [casts.mydeliberate]                               # explorer + batch synth (both tools)
            explorer = "gem/lite"
            synth    = { backend = "gem", id = "pro", lane = "batch" }

            [casts.mydirect]                                   # explorer + direct synth
            explorer = "gem/lite"
            synth    = { backend = "gem", id = "big", lane = "direct" }

            [casts.mydirect_synthonly]                         # offline, no explorer → no tool
            synth    = { backend = "gem", id = "big", lane = "direct" }
            "#,
        );
        let enum_of = |tool: &str| -> Vec<String> {
            h.tool_router
                .get(tool)
                .expect("tool advertised")
                .input_schema
                .get("properties")
                .and_then(|p| p.get("cast"))
                .and_then(|c| c.get("enum"))
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        // Each cast-taking tool's call-time acceptance, in one place beside the enum rules.
        let gate_accepts = |tool: &str, cast: &Cast| -> bool {
            match tool {
                "consult" | "consult_submit" | "oneshot" => {
                    h.reject_offline_cast(cast, tool).is_ok()
                }
                // `explore` has no lane gate — it runs whichever cast's explorer, so any cast
                // is accepted (a missing explorer faults later at the arm resolve, not the gate).
                "explore" => true,
                "batch_submit" => h.require_batch_cast(cast).is_ok(),
                "deliberate" => h.require_deliberate_cast(cast).is_ok(),
                other => panic!("unmapped cast-taking tool `{other}` — add its gate here"),
            }
        };

        let mut checked = 0;
        for &(tools, _) in CAST_ENUM_RULES {
            for &tool in tools {
                let advertised = enum_of(tool);
                assert!(
                    !advertised.is_empty(),
                    "the fixture must exercise `{tool}` — its enum is empty, so the guard is vacuous"
                );
                for name in advertised {
                    let cast = h
                        .config
                        .resolve_cast(&name)
                        .expect("advertised cast resolves");
                    assert!(
                        gate_accepts(tool, cast),
                        "tool `{tool}` advertises cast `{name}`, but its gate rejects it — \
                         the enum and the gate have drifted"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "the guard checked nothing");
    }

    /// The completeness half of the single source: every advertised tool that TAKES a
    /// `cast` argument must be covered by a `CAST_ENUM_RULES` entry — otherwise a future
    /// cast-taking tool would ship with a silently-empty `cast` enum (never advertising its
    /// roster, since `inject_cast_enum` is only called for tools named in the table). Reads
    /// the shipped schemas (a `cast` *property* is present whether or not the enum is
    /// populated), so adding a `cast` param without a rule fails here.
    #[test]
    fn every_cast_taking_tool_has_an_enum_rule() {
        let h = handler();
        let ruled: std::collections::HashSet<&str> = CAST_ENUM_RULES
            .iter()
            .flat_map(|(tools, _)| tools.iter().copied())
            .collect();
        let mut cast_taking = 0;
        for tool in h.advertised_tools() {
            let takes_cast = h
                .tool_router
                .get(&tool)
                .and_then(|t| t.input_schema.get("properties"))
                .and_then(|p| p.get("cast"))
                .is_some();
            if takes_cast {
                cast_taking += 1;
                assert!(
                    ruled.contains(tool.as_str()),
                    "tool `{tool}` takes a `cast` arg but no CAST_ENUM_RULES entry advertises \
                     its roster — its enum would ship empty"
                );
            }
        }
        assert!(
            cast_taking > 0,
            "no cast-taking tool found — the guard is vacuous"
        );
    }

    /// The lane gate's two halves, tested directly: an interactive tool refuses an
    /// offline cast (batch OR direct, naming the cast and the right route), and
    /// `batch_submit` refuses both a non-batch (interactive) cast and a `direct` cast
    /// with a distinct honest message — while each accepts the cast that fits its lane.
    #[test]
    fn lane_gate_refuses_the_wrong_lane() {
        let h = handler_from_toml(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.mydirect]
            synth = { backend = "gem", id = "some-big-local", lane = "direct" }
            "#,
        );
        let batch = h.config.resolve_cast("gemini-batch").unwrap().clone();
        let direct = h.config.resolve_cast("mydirect").unwrap().clone();
        let interactive = h.config.resolve_cast("anthropic").unwrap().clone();

        let err = h
            .reject_offline_cast(&batch, "consult")
            .expect_err("an interactive tool must refuse a batch cast");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("gemini-batch") && msg.contains("batch_submit"),
            "refusal should name the cast and point at batch_submit, got: {msg}"
        );

        let err = h
            .reject_offline_cast(&direct, "consult")
            .expect_err("an interactive tool must refuse a direct cast too");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("mydirect") && msg.contains("direct"),
            "refusal should name the cast and its lane, got: {msg}"
        );
        assert!(h.reject_offline_cast(&interactive, "consult").is_ok());

        let err = h
            .require_batch_cast(&interactive)
            .expect_err("batch_submit must refuse a non-batch cast");
        assert!(
            format!("{err:?}").contains("not a batch cast"),
            "refusal should explain the cast isn't a batch cast"
        );

        let err = h
            .require_batch_cast(&direct)
            .expect_err("batch_submit must refuse a direct-lane cast, not treat it as batch");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("mydirect") && msg.contains("direct") && msg.contains("batch"),
            "refusal should name the cast and explain it's direct, not batch, got: {msg}"
        );
        assert!(h.require_batch_cast(&batch).is_ok());

        // `deliberate` needs an OFFLINE synth (batch OR direct). It only checks the synth
        // lane here — the missing-explorer half is caught at the explorer arm resolve — so
        // an interactive cast is refused (pointed at consult) while both offline lanes pass.
        let err = h
            .require_deliberate_cast(&interactive)
            .expect_err("deliberate must refuse a cast with an interactive synth");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("anthropic") && msg.contains("consult"),
            "refusal should name the cast and point at consult, got: {msg}"
        );
        assert!(
            h.require_deliberate_cast(&batch).is_ok(),
            "a batch synth is an offline synth — the explorer gap (if any) is the arm's job"
        );
        assert!(
            h.require_deliberate_cast(&direct).is_ok(),
            "a direct synth is an offline synth — deliberate's local lane"
        );
    }

    /// `deliberate` is a third producer of async handles (a `backend/id` batch on its
    /// batch lane, a `job-N` on its direct lane), so the runtime collect-guards must keep
    /// its handles collectible even with `--no-consult --no-batch` — the per-handle mirror
    /// of the advertisement test in tests/gating.rs. And with deliberate *also* off, no
    /// producer remains, so both guards refuse.
    #[test]
    fn deliberate_keeps_its_handles_collectible() {
        let deliberate_only = |deliberate: bool| {
            let mut config = Config::builtin();
            config.tools = ToolGating {
                consult: false,
                batch: false,
                deliberate,
                explore: true,
                oneshot: true,
                run_kaish: true,
            };
            KaiboHandler::new(config).expect("handler builds")
        };

        // deliberate on, consult+batch off: both handle shapes stay collectible.
        let h = deliberate_only(true);
        assert!(
            h.ensure_batch_enabled("anthropic/msgbatch_x").is_ok(),
            "a deliberate batch handle must stay collectible with --no-batch"
        );
        assert!(
            h.ensure_consult_enabled("job-1").is_ok(),
            "a deliberate direct `job-N` must stay collectible with --no-consult"
        );

        // deliberate off too: no producer remains, so each guard refuses its shape.
        let off = deliberate_only(false);
        assert!(
            off.ensure_batch_enabled("anthropic/msgbatch_x").is_err(),
            "with every batch producer off, a batch handle is refused"
        );
        assert!(
            off.ensure_consult_enabled("job-1").is_err(),
            "with every job producer off, a `job-N` is refused"
        );
    }

    /// The lane-capture invariant, pinned: a per-call `synth_model` override retargets the
    /// model but must NOT change deliberate's offline lane. `apply_model_override` replaces
    /// the synth slot with a bare (laneless) one, so `deliberation_lane_with_overrides`
    /// captures the lane *before* overriding — this test fails (wrong lane, or a panic on
    /// the `.expect`) if that order is ever reversed. Also proves the capture is load-bearing:
    /// the slot really does go laneless, and the override really does take effect.
    #[test]
    fn deliberate_lane_survives_a_synth_model_override() {
        let h = handler_from_toml(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.mydeliberate]
            explorer = "gem/some-lite"
            synth    = { backend = "gem", id = "some-pro", lane = "batch" }
            "#,
        );
        let mut cast = h.config.resolve_cast("mydeliberate").unwrap().clone();
        // A synth_model override — this is what replaces the slot with a bare, laneless one.
        let lane = h
            .deliberation_lane_with_overrides(&mut cast, None, None, Some("some-other-pro"), None)
            .expect("override applies cleanly");
        assert_eq!(
            lane,
            Lane::Batch,
            "a synth_model override must not drop the batch lane"
        );
        // The capture was load-bearing: the override left the synth slot laneless...
        assert_eq!(
            cast.synth_lane(),
            None,
            "the override replaced the synth slot with a bare (laneless) one"
        );
        // ...and it did retarget the model.
        assert_eq!(
            cast.slot(ModelRole::Synth).map(|s| s.id.as_str()),
            Some("some-other-pro"),
            "the synth_model override took effect"
        );
    }

    /// The gate is wired into the live `batch_submit` handler and fires *before* any
    /// network: a non-batch cast is refused with no key and no provider call. (`consult`/
    /// `oneshot` wire the mirror gate the same way, right after `resolve_cast`.)
    #[tokio::test]
    async fn batch_submit_handler_refuses_a_non_batch_cast() {
        let h = handler();
        let err = h
            .batch_submit(Parameters(BatchSubmitInput {
                prompts: vec!["q".to_string()],
                attach: vec![],
                cast: Some("anthropic".to_string()),
                model: None,
                backend: None,
            }))
            .await
            .expect_err("batch_submit must refuse an interactive cast");
        assert!(
            format!("{err:?}").contains("not a batch cast"),
            "the handler must reject before building any provider client"
        );
    }

    /// `batch_submit` end to end, offline through the injected factory: the handler
    /// resolves the batch cast, mints the `backend/provider-id` handle, and hands each
    /// prompt to the provider as its own indexed item. Closes the handler-level gap the
    /// direct `batch::submitter` call left (the consult side already tests via `Arm::new`).
    #[tokio::test]
    async fn batch_submit_submits_through_the_injected_factory() {
        let scripted = Arc::new(crate::batch::ScriptedBatch::new("msgbatch_x", vec![]));
        let h = handler_from_toml(BATCH_CASTS_TOML).with_batch_providers(Arc::new(
            crate::batch::ScriptedBatchProviders(scripted.clone()),
        ));

        let out = h
            .batch_submit(Parameters(BatchSubmitInput {
                prompts: vec!["first".into(), "second".into()],
                attach: vec![],
                cast: Some("mybatch".into()),
                model: None,
                backend: None,
            }))
            .await
            .expect("scripted batch_submit succeeds");

        assert!(
            result_text(out).contains("gem/msgbatch_x"),
            "the reply namespaces the scripted id under the cast's backend"
        );
        // Both prompts reached the provider, one item each, indexed 0..N.
        let submits = scripted.submits();
        assert_eq!(submits.len(), 1, "one batch submitted");
        let items = &submits[0].2;
        assert_eq!(
            items.iter().map(|i| i.prompt.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(
            items
                .iter()
                .map(|i| i.custom_id.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1"],
            "items carry their index as custom_id"
        );
        // The offline-synth (batch) system prompt is submitted — not oneshot/consult's.
        assert_eq!(
            submits[0].0,
            crate::consult::batch_system_prompt(None),
            "the batch system prompt is passed through"
        );
    }

    /// `deliberate`'s BATCH lane, tested directly — no explorer, no network. `deliberate_batch`
    /// takes an already-built dossier and submits it as ONE item whose prompt is the
    /// `deliberation_prompt` (question + dossier), passing the offline-synth system prompt
    /// through and returning the durable handle. This is the batch-lane wiring `deliberate`
    /// added, now covered offline.
    #[tokio::test]
    async fn deliberate_batch_lane_submits_the_dossier_as_one_item() {
        let scripted = Arc::new(crate::batch::ScriptedBatch::new("msgbatch_d", vec![]));
        let h = handler_from_toml(BATCH_CASTS_TOML).with_batch_providers(Arc::new(
            crate::batch::ScriptedBatchProviders(scripted.clone()),
        ));
        let cast = h.config.resolve_cast("mydeliberate").unwrap().clone();

        let out = h
            .deliberate_batch(
                &cast,
                "gem/some-lite",
                "Is the retry safe?",
                "DOSSIER: src/x.rs:1 fn retry",
                "offline-synth-system",
            )
            .await
            .expect("scripted deliberate_batch succeeds");

        assert!(
            result_text(out).contains("gem/msgbatch_d"),
            "the reply carries the durable batch handle"
        );
        let submits = scripted.submits();
        assert_eq!(submits.len(), 1, "the dossier is one batch");
        let (system, attach, items) = &submits[0];
        assert_eq!(
            system, "offline-synth-system",
            "the system prompt passes through"
        );
        assert!(
            attach.is_empty(),
            "the offline submit carries no attachment parts — a deliberate `attach` \
             reaches the dossier stage as directives; the dossier is the prompt"
        );
        assert_eq!(items.len(), 1, "one item — the dossier, not fanned");
        assert_eq!(
            items[0].custom_id, "0",
            "the single dossier item is custom_id 0"
        );
        assert!(
            items[0].prompt.contains("Is the retry safe?")
                && items[0].prompt.contains("DOSSIER: src/x.rs:1 fn retry"),
            "the one item is the deliberation_prompt — question AND dossier: {}",
            items[0].prompt
        );
    }

    /// `job_get`'s batch arm polls through the factory: a scripted `Done` renders the
    /// item answers. Proves the collect path reaches the provider (not just the gate).
    #[tokio::test]
    async fn job_get_polls_a_batch_through_the_factory() {
        let scripted = Arc::new(crate::batch::ScriptedBatch::new(
            "msgbatch_x",
            vec![crate::batch::BatchPoll::Done(vec![
                crate::batch::BatchAnswer {
                    custom_id: "0".into(),
                    text: Ok("THE DELIBERATION".into()),
                },
            ])],
        ));
        let h = handler_from_toml(BATCH_CASTS_TOML).with_batch_providers(Arc::new(
            crate::batch::ScriptedBatchProviders(scripted.clone()),
        ));

        let out = h
            .job_get(Parameters(HandleInput {
                handle: "gem/msgbatch_x".into(),
            }))
            .await
            .expect("scripted job_get succeeds");
        assert!(
            result_text(out).contains("THE DELIBERATION"),
            "the batch's item answer is rendered"
        );
    }

    /// `job_cancel` and `job_list`'s batch section also route through the factory — cancel
    /// reaches the provider by id, and the listing renders the seeded batch.
    #[tokio::test]
    async fn job_cancel_and_list_reach_the_batch_through_the_factory() {
        let scripted = Arc::new(
            crate::batch::ScriptedBatch::new("msgbatch_x", vec![]).with_listing(
                vec![crate::batch::BatchListItem {
                    provider_id: "msgbatch_x".into(),
                    status: "running".into(),
                    completed: 0,
                    total: 1,
                    created_at: None,
                }],
                false,
            ),
        );
        let h = handler_from_toml(BATCH_CASTS_TOML).with_batch_providers(Arc::new(
            crate::batch::ScriptedBatchProviders(scripted.clone()),
        ));

        h.job_cancel(Parameters(HandleInput {
            handle: "gem/msgbatch_x".into(),
        }))
        .await
        .expect("scripted job_cancel succeeds");
        assert_eq!(
            scripted.canceled(),
            vec!["msgbatch_x".to_string()],
            "cancel reached the provider by id"
        );

        let out = h
            .job_list(Parameters(ListInput {
                all: false,
                backend: Some("gem".into()),
            }))
            .await
            .expect("scripted job_list succeeds");
        assert!(
            result_text(out).contains("msgbatch_x"),
            "the seeded batch appears in the listing"
        );
    }

    // (`job_wait`'s batch arm uses the same `batch_poller` choke-point these tests cover,
    // but the handler takes a live `Peer<RoleServer>` for its notification drain, so a full
    // offline handler test would need a fabricated peer — out of scope; the provider path
    // itself is proven above.)

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
    /// synth slot feeds *every* synth phase — the interactive `consult` driver, the
    /// toolless `oneshot`, AND the offline synth (`batch` / `deliberate`) — each via
    /// its own key (a copy today, free to diverge). The offline key matters: a
    /// batch/deliberate cast's synth *is* the offline synth, so its configured voice
    /// has to land there, not just on the interactive phases.
    #[test]
    fn slot_preamble_wins_over_phase_prompts_and_feeds_all_synth_phases() {
        let h = handler_from_toml(
            r#"
            [prompts]
            explorer = "EXP_PHASE"
            oneshot = "ONE_PHASE"
            consult = "CON_PHASE"
            batch = "BATCH_PHASE"

            [casts.team]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", preamble = "EXP_SLOT" }
            synth = { backend = "anthropic", id = "claude-opus-4-8", preamble = "SYNTH_SLOT" }
            "#,
        );
        let cast = h.resolve_cast(Some("team".into())).unwrap();
        let p = h.resolved_prompts(&cast);
        // Slot wins over the phase prompt for the explorer...
        assert_eq!(p.explorer.as_deref(), Some("EXP_SLOT"));
        // ...and the synth slot's voice reaches ALL synth phases, each via its own
        // key (a copy for now, independently addressable) — including the offline
        // synth that `batch`/`deliberate` run.
        assert_eq!(p.consult.as_deref(), Some("SYNTH_SLOT"));
        assert_eq!(p.oneshot.as_deref(), Some("SYNTH_SLOT"));
        assert_eq!(p.batch.as_deref(), Some("SYNTH_SLOT"));
    }

    /// With no slot preambles, the global `[prompts]` is the fallback — and the
    /// synth phases keep *independent* keys, so the toolless `oneshot`, the `consult`
    /// driver, and the offline `batch` synth can each differ.
    #[test]
    fn phase_prompts_are_the_fallback_and_synth_phases_stay_independent() {
        let h = handler_from_toml(
            r#"
            [prompts]
            oneshot = "ONESHOT_ONLY"
            consult = "DRIVER_ONLY"
            batch = "BATCH_ONLY"

            [casts.team]
            explorer = "anthropic/claude-haiku-4-5"
            synth = "anthropic/claude-opus-4-8"
            "#,
        );
        let cast = h.resolve_cast(Some("team".into())).unwrap();
        let p = h.resolved_prompts(&cast);
        assert!(p.explorer.is_none(), "no explorer prompt set anywhere");
        // The synth phases diverge — proof they're not collapsed into one.
        assert_eq!(p.oneshot.as_deref(), Some("ONESHOT_ONLY"));
        assert_eq!(p.consult.as_deref(), Some("DRIVER_ONLY"));
        assert_eq!(p.batch.as_deref(), Some("BATCH_ONLY"));
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

    /// A job's completion ping (a Warn carrying `job=<id>`) sits in the `job_wait` ring
    /// until drained. Collecting that job with `job_get` must retire its ping —
    /// otherwise the ping lingers and the next `job_wait` returns on it instantly
    /// instead of blocking for new work (the "`job_wait` returns too fast" bug). An
    /// *uncollected* job's ping is untouched, so it still wakes a later `job_wait`.
    #[tokio::test]
    async fn job_get_on_a_terminal_job_retires_its_wait_ping() {
        use crate::jobs::JobResult;
        use crate::mcp_log::LogRecord;
        use rmcp::model::LoggingLevel;

        fn ping(job: &str) -> LogRecord {
            let mut fields = serde_json::Map::new();
            fields.insert("job".into(), serde_json::Value::String(job.into()));
            LogRecord {
                level: LoggingLevel::Warning,
                target: "kaibo::jobs".into(),
                message: format!("async job finished — collect it with `job_get` ({job})"),
                fields,
            }
        }

        let h = handler();
        // Two finished jobs; we only collect the first.
        let collected = h
            .jobs
            .submit("test", Arc::new(ProgressLog::silent()), async {
                Ok(JobResult {
                    answer: "answer".into(),
                    report: None,
                })
            });
        let other = h
            .jobs
            .submit("test", Arc::new(ProgressLog::silent()), async {
                Ok(JobResult {
                    answer: "answer".into(),
                    report: None,
                })
            });
        // Both must reach a terminal state before `job_get` will evict (Running has no ping).
        for id in [&collected, &other] {
            for _ in 0..1000 {
                match h.jobs.get(id).map(|s| s.state) {
                    Some(JobState::Running) | None => tokio::task::yield_now().await,
                    Some(_) => break,
                }
            }
        }
        // Seed both pings, the way the finishing tasks' `tracing::warn!` would.
        h.notifications.push_record(ping(&collected));
        h.notifications.push_record(ping(&other));

        h.job_get(Parameters(HandleInput {
            handle: collected.clone(),
        }))
        .await
        .expect("job_get collects the finished job");

        // The collected job's ping is gone; the uncollected one's survives to wake a
        // later `job_wait`.
        let left: Vec<String> = h
            .notifications
            .drain(crate::mcp_log::rank(LoggingLevel::Warning), 20)
            .into_iter()
            .map(|r| {
                r.fields
                    .get("job")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(left, vec![other], "only the uncollected job's ping remains");
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

    /// The schemas point at `kaibo://tools` for the long-form guidance they no longer
    /// carry, so the resource must be both advertised (a client can discover it) and
    /// readable, and it must actually hold the guidance that moved out of the schemas:
    /// the attachment semantics, the override mechanics, and the async handle shapes. If
    /// any of these drift out of the doc, a caller following the schema's pointer lands
    /// on a page that no longer answers the question the terse schema deferred.
    #[test]
    fn tools_doc_is_advertised_and_carries_the_moved_guidance() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.raw.uri).collect();
        assert!(
            uris.iter().any(|u| u == TOOLS_URI),
            "must advertise the tools doc, got {uris:?}"
        );
        let text = read_text(TOOLS_URI, &[]);
        for needle in [
            "attach",              // the attachment guidance moved here
            "inlined",             // the consult-vs-oneshot attach distinction
            "whole file",          // the toolless-model whole-files steer
            "verbatim",            // the model-id override semantics
            "_backend",            // the retarget-the-slot mechanic
            "job-N",               // the consult handle shape
            "backend/provider-id", // the batch handle shape
            "fire-and-forget",     // the async-workflow framing
            "read-only",           // the kaish shell boundary
            "126",                 // the exit-code contract
            "worktree",            // attach/path reaches a followed git worktree
            "Reviewing a change",  // prefer whole files over a diff for review
            "view_image",          // consult opens an attached image with view_image
        ] {
            assert!(
                text.contains(needle),
                "tools doc must cover {needle:?}:\n{text}"
            );
        }
    }

    /// The `kaibo://prompts` resource must be advertised AND render each phase's system
    /// preamble *verbatim* — byte-identical to what the tools send, because both go
    /// through `resolve_phase_preamble`. Asserting the exact built-in bodies is the
    /// anti-drift guard: if a preamble is ever restated in the resource instead of
    /// rendered, these break. It must also carry the two dynamic user-turn framings and
    /// the layering note that names the per-call/per-cast layers it can't render.
    #[test]
    fn prompts_resource_is_advertised_and_renders_each_phase_verbatim() {
        use crate::consult::{
            batch_preamble, consult_preamble, deliberation_prompt, oneshot_preamble,
            report_preamble,
        };
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.raw.uri).collect();
        assert!(
            uris.iter().any(|u| u == PROMPTS_URI),
            "must advertise the prompts doc, got {uris:?}"
        );
        let text = read_text(PROMPTS_URI, &[]);
        // Each phase's built-in preamble appears verbatim (single-sourced — no drift).
        for body in [
            report_preamble(),
            consult_preamble(),
            oneshot_preamble(),
            batch_preamble(),
        ] {
            assert!(
                text.contains(&body),
                "prompts doc must render the phase preamble verbatim, missing:\n{body}"
            );
        }
        // The dynamic user-turn framing is rendered by the real code, not paraphrased.
        assert!(
            text.contains("Now answer the current question"),
            "must show the consult user-turn framing:\n{text}"
        );
        assert!(
            text.contains(&deliberation_prompt("<your question>", "")[..40]),
            "must show the deliberate user-turn framing:\n{text}"
        );
        // The layering note names what a static doc can't render per call/per cast, and
        // points at the per-cast resource for the resolved-per-slot view.
        for needle in [
            "[orientation]",
            "[context]",
            "per-slot",
            "kaibo://prompts/<cast>",
        ] {
            assert!(
                text.contains(needle),
                "prompts doc must name the {needle:?} layer:\n{text}"
            );
        }
        // A phase is a role several tools share — the doc says so explicitly, so a reader
        // knows tuning the explorer phase moves `deliberate`'s dossier pass too.
        assert!(
            text.contains("dossier-building pass") && text.contains("`batch_submit`"),
            "the doc must spell out which tools each shared phase drives:\n{text}"
        );
    }

    /// A global `[prompts]` override must show through the resource — its text rendered
    /// in that phase's section and flagged as an active override — while an un-overridden
    /// sibling still shows its built-in. Proves the doc reflects the operator's real
    /// config, not just the defaults.
    #[test]
    fn prompts_resource_reflects_a_prompts_override() {
        use crate::consult::oneshot_preamble;
        let config = Config::from_toml_str(
            r#"
            [prompts]
            consult = "MY CUSTOM CONSULT FRAME"
            "#,
        )
        .expect("config parses");
        let text = render_prompts_resource(&config, None);
        assert!(
            text.contains("MY CUSTOM CONSULT FRAME"),
            "overridden consult frame must render:\n{text}"
        );
        assert!(
            text.contains("global `[prompts]` override"),
            "the overridden phase must be flagged:\n{text}"
        );
        // The un-overridden oneshot still shows its built-in, tagged as such.
        assert!(
            text.contains(&oneshot_preamble()),
            "un-overridden phase keeps its built-in:\n{text}"
        );
        assert!(
            text.contains("kaibo built-in"),
            "an un-overridden phase must be tagged built-in:\n{text}"
        );
    }

    /// `kaibo://prompts/<cast>` resolves *that cast's* framing: a synth slot's `preamble`
    /// renders across all three synth phases (consult, oneshot, batch) and is attributed
    /// to the slot; an explorer slot's `preamble` frames the explorer phase; and the
    /// per-cast doc drops the (cast-independent) user-turn section, pointing back instead.
    #[test]
    fn per_cast_prompts_resource_folds_in_the_slot_preambles() {
        let config = Config::from_toml_str(
            r#"
            [casts.team]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", preamble = "EXPLORER SLOT VOICE" }
            synth = { backend = "anthropic", id = "claude-opus-4-8", preamble = "SYNTH SLOT VOICE" }
            "#,
        )
        .expect("config parses");
        let cast = config.resolve_cast("team").expect("team cast exists");
        let text = render_prompts_resource(&config, Some(cast));
        // The synth slot's voice reaches all three synth phases...
        assert_eq!(
            text.matches("SYNTH SLOT VOICE").count(),
            3,
            "synth slot preamble must render in consult + oneshot + batch:\n{text}"
        );
        // ...and the explorer slot frames the explorer phase.
        assert!(
            text.contains("EXPLORER SLOT VOICE"),
            "explorer slot preamble must render:\n{text}"
        );
        // Each is attributed to the slot that set it (not "global override" / "built-in").
        assert!(
            text.contains("cast `team` slot `preamble`"),
            "a slot-framed phase must be tagged to the cast slot:\n{text}"
        );
        // The user-turn framing lives on the cast-independent doc only.
        assert!(
            !text.contains("## User-turn framing"),
            "per-cast doc must not repeat the user-turn framing:\n{text}"
        );
        assert!(
            text.contains("kaibo://prompts"),
            "per-cast doc must point back to the base doc:\n{text}"
        );
    }

    /// The per-cast template is advertised, and an unknown cast is a not-found whose
    /// message names the known casts (so a caller recovers to a real cast name).
    #[test]
    fn per_cast_prompts_template_advertised_and_unknown_cast_is_not_found() {
        let templates: Vec<String> = kaibo_resource_templates()
            .into_iter()
            .map(|t| t.raw.uri_template)
            .collect();
        assert!(
            templates.iter().any(|t| t == PROMPTS_CAST_URI_TEMPLATE),
            "must advertise the per-cast prompts template, got {templates:?}"
        );
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = Vec::new();
        let err = read_kaibo_resource_with_config(
            "kaibo://prompts/nope-not-a-cast",
            &[],
            &config,
            &allowed,
            None,
            false,
            vec![],
        )
        .expect_err("an unknown cast must be a not-found");
        assert!(
            err.message.contains("nope-not-a-cast") && err.message.contains("known casts"),
            "not-found must name the bad cast and the roster, got: {}",
            err.message
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
        let allowed = vec![
            std::path::PathBuf::from("/projects/myapp"),
            std::path::PathBuf::from("/data/shared"),
        ];
        let config = Config::builtin();
        let text = kaibo_instructions_with_scope(
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
        let config = Config::builtin();
        let root = std::path::PathBuf::from("/projects/myapp");
        let allowed = vec![root.clone()];
        let text = kaibo_instructions_with_scope(
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
        let config = Config::builtin();
        let root = std::path::PathBuf::from("/work/space");
        let allowed = vec![root.clone()];
        let text = kaibo_instructions_with_scope(
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
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp")];
        let text = kaibo_instructions_with_scope(
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
            synth = { backend = "openai-local", id = "llava", vision = true, max_tokens = 999 }
            "#,
        )
        .unwrap();
        let h = KaiboHandler::new(config).unwrap();
        let mut cast = h.resolve_cast(Some("pinned".into())).unwrap();
        h.override_model(&mut cast, ModelRole::Synth, "other-model", None)
            .unwrap();
        let slot = cast.slot(ModelRole::Synth).unwrap();
        assert_eq!(slot.backend, "openai-local", "backend kept");
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
        let mut cast = h.resolve_cast(Some("openai-local".into())).unwrap();
        h.override_model(
            &mut cast,
            ModelRole::Explorer,
            "google/gemma-3-27b-it",
            None,
        )
        .unwrap();
        let slot = cast.slot(ModelRole::Explorer).unwrap();
        assert_eq!(
            slot.backend, "openai-local",
            "the configured backend is kept"
        );
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
