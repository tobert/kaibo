//! The MCP server surface: one `consult` tool over the two-phase pipeline.
//!
//! stdio only — like kaish-mcp, kaibo must never bind a socket: it can read a
//! user's filesystem, so the transport pipe is the security boundary.
//!
//! `LoggingLevel`, `SetLevelRequestParams`, `enable_logging()`,
//! `notify_logging_message`, and `LoggingMessageNotificationParam` are
//! SEP-2577-deprecated upstream, but the MCP logging channel is still kaibo's
//! live logging surface — there is no replacement yet. We
//! `#[expect(deprecated)]` the module until a successor ships.
#![expect(deprecated)]

use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use kaish_kernel::tools::ToolSchema;
use rig_core::completion::Usage;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResponse,
    GetPromptResult, Implementation, JsonObject, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, LoggingLevel, MetaObject, PaginatedRequestParams,
    ProgressNotificationParam, ProgressToken, Prompt, PromptArgument, PromptMessage,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    RequestMetaObject, Resource, ResourceContents, ResourceTemplate, ResultType, Role,
    ServerCapabilities, ServerInfo, SetLevelRequestParams,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{Peer, RequestContext};
use rmcp::ErrorData as McpError;
use rmcp::{tool, tool_handler, tool_router, RoleServer};
use serde::Deserialize;
use tracing::Instrument;

use crate::config::{Backend, Cast, Config, Lane, ModelRole, ModelSlot};
use crate::consult::{
    consult, explore_with, oneshot, sweep_evidence_block, Arm, ConsultConfig, ExploreConfig,
    ModelCaps, PhaseContext, PromptOverrides,
};
use crate::explorer::format_output;
use crate::jobs::{CancelOutcome, JobResult, JobState, JobStore};
use crate::kaish_syntax::{
    kaibo_instructions_with_scope, kaibo_sandbox_doc, render_builtin_help, render_topic, topics,
};
use crate::mcp_log;
use crate::progress::{NullSink, PhaseEvent, ProgressLog, ProgressSink, TracingSink};
use crate::sandbox::{builtin_schemas, KaishWorker};
use crate::session::{SessionStore, Sessions};
use crate::sweep_attach::{SweepAttachSink, SweepConsumer, SweepConsumerKind};

mod cas_read;
mod config_resource;
mod containment;
mod dossier;
pub(crate) mod render;
mod resolver;

pub use resolver::Resolver;

// Re-exported for the CLI front door (`crate::cli`), which renders the same answer
// footer, failure text, `kaibo://config` document, and `batch list` recency window the
// MCP handler does.
// The CLI reads artifacts by digest too (`kaibo cas read`), and a read's RULES —
// what leads, what is bounded, what is never dumped as base64 — belong to one planner,
// not to whichever front door asked.
pub(crate) use cas_read::{plan as plan_cas_read, Body as CasBody, CasObject, Delivery};
pub(crate) use config_resource::render_config_resource;
// `deliberate` is the same two stages on either road, so the CLI borrows the pieces
// rather than growing a second copy of them: how a dossier is kept and loaded, how it is
// named back to the caller, which explorer arguments a reuse call must refuse, and the
// direct lane's wall-clock backstop.
pub(crate) use dossier::{
    dossier_ack, inert_explorer_args, keep_dossier, load_dossier, with_dossier, ExplorerArgs,
    KeptDossier,
};
use render::{
    batch_poll_brief, consult_answer_text, consult_result, consultation_failed,
    consultation_failed_with_artifacts, consultation_failure_text_with_artifacts, fmt_usage,
    is_batch_handle, parse_batch_handle, render_job, render_jobs_section, render_wait,
    wait_level_floor, wait_level_label,
};
pub(crate) use render::{
    batch_within_window, consultation_failure_text, now_epoch_secs, with_provenance,
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
/// The full configuration reference manual (`docs/config.md`). The third config
/// resource: `kaibo://config` is the resolved state, `kaibo://config/example` the
/// copyable template, and this the explanatory prose behind both — precedence,
/// backend/cast design, tool gating, containment, persistence. It exists so the
/// template can stay a *template*: an agent configuring kaibo over MCP has no access to
/// this repo's `docs/`, so without it every explanation had to be smuggled into the
/// example's comments, where it costs bytes on every read and drifts from the code.
const CONFIG_GUIDE_URI: &str = "kaibo://config/guide";
/// Long-form "how to wield the tools well" guidance — attachments, cast/model
/// selection, the sync↔async pairs and their handles, and the read-only shell's
/// idioms. The tool schemas stay terse and point here, so the repetition and positive
/// framing that helps a calling model use the tools lives in a resource the host loads
/// on demand, not in every agent's startup context (the AGENTS.md prompt-writing split:
/// terse where it's always loaded, generous where it's pulled).
const TOOLS_URI: &str = "kaibo://tools";
/// How an artifact is NAMED wherever kaibo prints one: `kaibo://cas/<sha256-hex>`.
///
/// It is an identifier, not a route. It was an MCP resource until 2026-08-05; retrieval
/// is now the `read_cas` tool, which takes the digest out of this string (see
/// [`cas_read`]'s module doc for why a tool and not a resource). The string stays because
/// footers, answers, and `generate` results all need a stable name for an artifact.
///
/// OPERATOR SURFACE ONLY (Amy's ruling, 2026-08-03), and that survived the move: the MCP
/// client retrieves, through `read_cas`; the inner model team never can — the CAS is not
/// mounted into kaish and no cast-facing tool reads it, because kaibo state spans projects
/// and a browsable CAS would let one project's team enumerate another's artifacts. (kaibo's
/// CLI has no artifact command at all today; on disk, the metadata's path is what an
/// operator's own tools use.)
const CAS_RES_PREFIX: &str = crate::cas::CAS_URI_PREFIX;
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
/// `pub(crate)` so `kaibo example-config` (`cli.rs`) can print the exact same string
/// the `kaibo://config/example` resource serves — one source, no drift.
pub(crate) const CONFIG_EXAMPLE_TOML: &str = include_str!("../../docs/config.example.toml");

/// `docs/config.md`, embedded for the same reason as the template above: `cargo install
/// kaibo` lays down no docs, so a runtime file read would 404 exactly when someone is
/// trying to configure the thing.
const CONFIG_GUIDE_MD: &str = include_str!("../../docs/config.md");

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
pub(crate) fn deliberate_direct_deadline(synth_backend: &Backend) -> std::time::Duration {
    synth_backend.request_timeout + DELIBERATE_DEADLINE_MARGIN
}

/// Fold a `deliberate` dossier sweep's routed `attach` delivery into the dossier and
/// hand back the routed images for the lane dispatch.
///
/// Text bodies, notes, and demotions become dossier text ([`sweep_evidence_block`]);
/// images are returned separately because they ride the offline synth's single turn
/// as native image parts, never as dossier text. `None` (attach disabled, or a sweep
/// that routed nothing) leaves the dossier untouched and returns no images.
///
/// Extracted from the `deliberate` handler so the drain → stitch → images hand-off is
/// pinned by a test: the images the sweep routed MUST be what the lane dispatch
/// receives, and a refactor that drops them between drain and dispatch fails the
/// suite instead of silently blinding the synth (DeepSeek cross-family review,
/// 2026-07-26).
fn stitch_dossier_delivery(
    dossier: &mut String,
    consumer: &SweepConsumer,
    sink: Option<&std::sync::Arc<SweepAttachSink>>,
) -> Vec<crate::attach::Attachment> {
    match sink {
        Some(sink) => {
            let delivery = sink.drain();
            if let Some(block) = sweep_evidence_block(consumer, &delivery) {
                dossier.push_str(&block);
            }
            delivery.images()
        }
        None => Vec::new(),
    }
}

/// Stitch a dossier sweep's delivery, then keep the finished dossier — in that order.
///
/// The order is the whole reason this is one function. [`stitch_dossier_delivery`]
/// *appends* the routed text bodies to the dossier, so keeping it first would store a
/// dossier missing evidence the synth then reasons over — an audit record that quietly
/// disagrees with what was actually sent, which is worse than keeping nothing. Composed
/// here so a test can pin the order (DeepSeek cross-family review, 2026-08-07); the
/// handler calls this rather than the two steps by hand.
pub(crate) fn stitch_and_keep(
    dossier: &mut String,
    consumer: &SweepConsumer,
    sink: Option<&std::sync::Arc<SweepAttachSink>>,
    store: Option<&Arc<crate::cas::MediaStore>>,
    question: &str,
    cast: &str,
    explorer_model: &str,
) -> (Vec<crate::attach::Attachment>, Option<KeptDossier>) {
    let images = stitch_dossier_delivery(dossier, consumer, sink);
    let kept = keep_dossier(store, question, dossier, cast, explorer_model);
    (images, kept)
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
    /// Read-only model discovery (`list_models`) — an operator/config surface, not a
    /// model-driven tool: no cast, no tool loop, just a GET per backend. Its own gate,
    /// independent of the model-backed tools above.
    pub list_models: bool,
    /// The `generate` tool (media generation through a cast's `image` slot) — its own
    /// gate. Beyond this flag it also needs a cast that can staff it AND the media CAS
    /// on (`[cas] enabled`), since artifacts must have somewhere to land — see
    /// `live_tools`.
    pub generate: bool,
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
            list_models: true,
            generate: true,
        }
    }
}

impl ToolGating {
    /// Whether the operator left tool `name` enabled — the raw `--no-<tool>` answer,
    /// *before* the staffing gate in [`KaiboHandler::new`] has its say. Used to tell the
    /// two reasons a tool can vanish apart: a flag-off tool is the operator's own choice
    /// and needs no explanation, while a flag-ON tool no cast can staff earns a startup
    /// warning. `consult_submit` shares `consult`'s flag (one capability, two shapes).
    /// A name with no flag of its own reads as enabled — the castless tools
    /// (`run_kaish`, `list_models`) are gated directly, never through this.
    pub fn enabled(&self, name: &str) -> bool {
        match name {
            "consult" | "consult_submit" => self.consult,
            "explore" => self.explore,
            "deliberate" => self.deliberate,
            "oneshot" => self.oneshot,
            "run_kaish" => self.run_kaish,
            "batch_submit" => self.batch,
            "list_models" => self.list_models,
            "generate" => self.generate,
            _ => true,
        }
    }

    /// True iff every tool is disabled — the zero-tool server we refuse to start.
    pub fn all_disabled(&self) -> bool {
        !self.consult
            && !self.explore
            && !self.deliberate
            && !self.oneshot
            && !self.run_kaish
            && !self.batch
            && !self.list_models
            && !self.generate
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
    /// root; must be at-or-under an allowed tree, or a linked git worktree of one
    /// (`kaibo://config` shows the set).
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

    /// Override the synthesis agent's (final-answer) model id. See `kaibo://tools` for override
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

    /// Let this consult save bulk output (a generated corpus, a long inventory) into
    /// kaibo's artifact store instead of the answer; the answer names each
    /// `kaibo://cas/<digest>` to read. Off by default, and the server must allow it.
    #[serde(default)]
    pub save_artifacts: bool,
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

    /// Workspace files (under the project root) central to the survey: the explorer
    /// is directed to read each one WHOLE as part of its sweep. Text only — it reads
    /// through the shell, so attach images to `consult` with a vision cast instead.
    #[serde(default)]
    pub attach: Vec<String>,

    /// Absolute path to the project to explore. Optional when the server has a default
    /// root; must be at-or-under an allowed tree, or a linked git worktree of one
    /// (`kaibo://config` shows the set).
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
    /// be at-or-under an allowed tree, or a linked git worktree of one (`kaibo://config`
    /// shows the set).
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

    /// Reuse a dossier kaibo already built instead of sweeping for a new one: pass the
    /// digest (bare, or as its `kaibo://cas/<digest>` URI) that an earlier `deliberate`
    /// handed back. No explorer runs, so this call costs only the synth — the way to put
    /// the same evidence in front of a second cast. The explorer arguments (`attach`,
    /// `explorer_model`, `explorer_backend`, `explorer_max_turns`) have nothing to act on
    /// here, and are refused rather than ignored.
    #[serde(default)]
    pub dossier: Option<String>,
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

    /// Max records to sample into the result (default 20, newest activity last — when more
    /// happened than this, you get the most recent tail).
    #[serde(default)]
    pub limit: Option<usize>,

    /// How much narrative rides back in the result — *not* when the call returns. It always
    /// parks up to `timeout_secs` and returns early only when a job finishes or fails (a
    /// real event), then hands back a sample of what happened. `warn` (default) is just the
    /// flagged milestones; `info` folds in the watchable narrative (each kaish command,
    /// sweep, milestone) so you can follow along; `debug` is everything; `error` trims to
    /// failures. A richer level never makes the call return sooner — it only fills the tail.
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
    /// at-or-under an allowed tree, or a linked git worktree of one (`kaibo://config`
    /// shows the set). Each call starts fresh at this root — there is no persistent cwd
    /// across calls.
    #[serde(default)]
    pub path: Option<String>,
}

/// Arguments to `list_models`: an operator/config-surface tool, not a model-driven one
/// (no cast, no tool loop — see `discover.rs`'s module doc). Omit `backend` to sweep
/// every configured backend.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListModelsInput {
    /// Which backend (name or alias) to query. Omit to sweep every configured backend.
    /// `kaibo://config` lists the known backends.
    #[serde(default)]
    pub backend: Option<String>,
}

/// Arguments for [`KaiboHandler::read_cas`] — a digest and an optional byte range.
///
/// No path of any kind, in either direction: the address is the content hash, so there is
/// nothing to aim. The range exists because `resources/read`, which this replaces, had no
/// way to ask for less than everything.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadCasInput {
    /// The artifact's content digest: 64 lowercase hex, exactly as a `generate` result or
    /// a consult's artifact footer prints it (the `kaibo://cas/<digest>` tail).
    pub digest: String,

    /// Byte offset to read from. Default 0. Past the end is an empty range with the
    /// metadata, not an error — that is how a paging loop learns where the object ended.
    #[serde(default)]
    pub offset: Option<usize>,

    /// How many bytes to read, capped at 1048576 — a larger ask is refused, not trimmed.
    /// `0` is metadata only. Omit it for the per-kind default: up to 65536 bytes of TEXT
    /// from `offset`, a whole image when one qualifies, and metadata alone for any other
    /// binary (a base64 range is served only when you ask for one).
    #[serde(default)]
    pub length: Option<usize>,
}

/// Arguments for [`KaiboHandler::write_cas`] — where the image is, and an optional label.
///
/// Exactly one of `path` and `content`. There is no destination of any kind (the address
/// is the content hash) and no `mime` (the format is read out of the bytes, so there is
/// no claim that can be wrong). A source `path` is read-scoped to the allowed set and
/// read through the read-only kaish VFS. See [`crate::upload`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteCasInput {
    /// Path to the image file to store. Relative paths resolve against the launch
    /// directory. Must be a regular file inside the allowed set. This is the route to
    /// use whenever the image is on disk.
    #[serde(default)]
    pub path: Option<String>,

    /// The image's raw bytes, base64-encoded — for an image that is not a file (pasted
    /// into a conversation, or held in memory). Prefer `path`: these bytes are tokens
    /// you have to write out, so a real screenshot costs far more here than a path does.
    #[serde(default)]
    pub content: Option<String>,

    /// One short line describing the image, recorded beside it and shown by `read_cas`.
    /// Optional, capped at 200 bytes, single-line.
    #[serde(default)]
    pub label: Option<String>,
}

/// Arguments to `generate`: media generation through the cast's `image` slot. No
/// `path` — generation reads no project (the prompt is the whole input), so there is
/// nothing to scope to an allowed tree.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateInput {
    /// What to generate, described in prose.
    pub prompt: String,

    /// Cast (model team) whose `image` slot generates. Optional — the server default
    /// applies; the cast must carry an `image` slot (kaibo://config lists casts).
    #[serde(default)]
    pub cast: Option<String>,

    /// Provider-native generation options passed through verbatim — each value's
    /// JSON type (string | number | boolean) is the type the provider receives, so
    /// pass `n` as a number and ids like `user` as strings. (Stability:
    /// aspect_ratio "16:9", output_format png|jpeg|webp, seed, negative_prompt,
    /// style_preset, ...; OpenAI-compatible images endpoints: size "1024x1024", n,
    /// quality, output_format png|jpeg|webp, ...). The provider validates its own
    /// knobs. `prompt` and `model` are reserved: use the prompt parameter and the
    /// cast's image slot.
    #[serde(default)]
    pub fields: Option<std::collections::BTreeMap<String, GenerateFieldValue>>,

    /// Input images for the operations that take them, as `{form-field: digest}` —
    /// e.g. `{"image": "<digest>"}` for image-to-image, `{"image": "...", "mask":
    /// "..."}` where an operation takes both. Each digest is one `write_cas` or
    /// `generate` handed back, so an image already in kaibo's store is reused by
    /// address and never re-sent. The form-field names are the provider's own; the
    /// store decides each part's format, not you.
    #[serde(default)]
    pub inputs: Option<std::collections::BTreeMap<String, String>>,

    /// Which operation to run, when the backend has more than one. Omit it to generate
    /// from the prompt alone. Stability's operations and what each costs in credits (one
    /// credit is about a US cent, so `upscale/conservative` is twenty times
    /// `generate/core`): `edit/erase` 5 (image; optional `mask`, or an alpha channel on the image),
    /// `edit/inpaint` 5 (image; optional `mask`, or an alpha channel on the image),
    /// `edit/outpaint` 4 (image), `edit/search-and-replace` 5 (image, `search_prompt`),
    /// `edit/search-and-recolor` 5 (image, `select_prompt`), `edit/remove-background` 5
    /// (image), `control/sketch` 5 (image), `control/structure` 5 (image),
    /// `control/style` 5 (image), `control/style-transfer` 8 (init_image+style_image),
    /// `upscale/fast` 2 (image), `upscale/conservative` 40 (image). The parenthesised
    /// names are the `inputs` keys that operation needs.
    #[serde(default)]
    pub op: Option<String>,
}

/// One `generate` field value, as typed JSON: the schema face of
/// [`crate::media::FieldValue`]. Untagged, so a caller writes plain JSON scalars
/// (`{"n": 2, "size": "1024x1024", "transparent": true}`) and the stated type — not
/// a guess re-derived from text — rides through to the provider's wire.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum GenerateFieldValue {
    Bool(bool),
    /// `serde_json::Number` keeps integers integral end to end (`2`, never `2.0`).
    Num(serde_json::Number),
    Str(String),
}

impl GenerateFieldValue {
    fn into_field_value(self) -> crate::media::FieldValue {
        match self {
            GenerateFieldValue::Str(s) => crate::media::FieldValue::Str(s),
            GenerateFieldValue::Num(n) => crate::media::FieldValue::Num(n),
            GenerateFieldValue::Bool(b) => crate::media::FieldValue::Bool(b),
        }
    }
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
    /// Multi-turn `consult` sessions, behind the [`Sessions`] seam: the in-memory LRU by
    /// default, or the durable turso-backed store when `[persistence]` is enabled (swapped
    /// in by [`with_session_store`](Self::with_session_store)). Cheap `Clone` either way, so
    /// the per-request handler clones share one backend. The persistent variant also carries
    /// the batch-handle store ([`Sessions::store`]).
    sessions: Sessions,
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
    /// The shared resolution glue both front doors run (`resolve_root`,
    /// `resolve_cast`, model overrides, `arm`, house rules / orientation / prompts,
    /// attachment resolution + vision gates) plus the canonicalized containment
    /// boundary (allowed set + inferred default root). Extracted so the CLI front door
    /// runs the *identical* resolution without a full handler; the handler keeps thin
    /// delegating shims over it (see [`Resolver`]). Cheap `Clone` (all `Arc`).
    resolver: Resolver,
    /// How the batch handlers build provider clients — the injection seam. `new` seeds
    /// [`LiveBatchProviders`](crate::batch::LiveBatchProviders) (the real network
    /// builders); tests swap in a scripted double via
    /// [`with_batch_providers`](Self::with_batch_providers) to exercise the submit/poll
    /// handler wiring offline. `Arc<dyn …>` so the derived `Clone` shares one factory.
    batch_providers: Arc<dyn crate::batch::BatchProviderFactory>,
    /// The media CAS, in whichever mode the lifecycle resolved (see
    /// [`Config::cas_mode`]): `None` when `[cas] enabled = false`. `new` seeds the
    /// in-memory store (mirroring how sessions start in-memory);
    /// [`finalize_media_store`](Self::finalize_media_store) upgrades it to the disk
    /// store once `main` knows whether persistence actually came up. `Arc` so
    /// per-request handler clones share one store.
    media_cas: Option<Arc<crate::cas::MediaStore>>,
    /// How the CAS directory's backing filesystem is probed at startup. A field rather
    /// than a direct call so a test can script the answer — what filesystem the test
    /// process happens to be running on is the one thing it cannot arrange portably.
    backing_probe: crate::cas::BackingProbe,
    /// The ephemeral filesystem the CAS turned out to sit on, if any — set by
    /// [`finalize_media_store`](Self::finalize_media_store) in disk mode, and reported by
    /// `kaibo://config` under `[cas] backing`.
    ///
    /// The startup warning is the loud channel, but startup log is exactly the thing an
    /// operator scrolls past or never sees (a client launched kaibo for them). Leaving
    /// the finding somewhere queryable is what turns "we told you" into "you can check" —
    /// the same reason memory mode reports `mode = "memory"` rather than only warning.
    cas_ephemeral_fs: Option<&'static str>,
    /// How `generate` builds its media arm — the injection seam mirroring
    /// `batch_providers`: production seeds [`crate::media::LiveMediaArms`]; tests swap
    /// in a factory returning a scripted model via
    /// [`with_media_arms`](Self::with_media_arms).
    media_arms: Arc<dyn crate::media::MediaArmFactory>,
}

/// One `CAST_ENUM_RULES` entry: the tools sharing a cast eligibility, the predicate
/// (a `Config::cast_is_*`/`cast_can_*`) that decides which usable casts they advertise,
/// and a plain-language statement of the cast *shape* the predicate wants.
///
/// That third field is operator-facing text, not decoration: when a rule matches nothing
/// the tools are dropped entirely (see [`KaiboHandler::new`]), and vanishing silently is
/// right for the model's tool list but wrong for the operator — so the startup warning
/// says what a cast would have to look like to bring the tool back. Keeping it in this
/// table rather than at the log site is what stops the message from drifting away from
/// the predicate it describes.
type CastEnumRule = (
    &'static [&'static str],
    fn(&Config, &str) -> bool,
    &'static str,
);

/// The single source mapping each cast-taking tool to the predicate that decides which
/// *usable* casts its `cast` enum advertises — keyed on the cast's shape (synth lane +
/// explorer, via the `Config::cast_is_*`/`cast_can_*` predicates). [`KaiboHandler::new`]
/// injects the enums straight from this table. A cast may match more than one rule (a
/// deliberate-shaped batch cast like `fable` serves both `batch_submit` and `deliberate`);
/// the rules are independent filters, not a partition.
///
/// This table also decides whether a tool is advertised **at all**: a rule matching no
/// usable cast means nothing can staff its tools, so [`KaiboHandler::new`] removes their
/// routes instead of shipping a tool whose every call would fail. See that function for
/// why removal beats an empty enum.
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
        "a cast whose synth answers interactively (any cast without an offline synth lane)",
    ),
    // `explore` runs only the explorer, so it advertises *any* cast with one — including
    // `deliberate`/`direct` casts, whose (often smarter) explorers are useful standalone.
    // Its own rule, broader than the interactive tools' (which also need an interactive synth).
    (
        &["explore"],
        Config::cast_can_explore,
        "a cast with an `explorer` slot",
    ),
    (
        &["batch_submit"],
        Config::cast_is_batch,
        "a cast whose synth runs on the batch lane (`lane = \"batch\"`, or the `batch = true` \
         sugar) — Anthropic, Gemini, or a hosted OpenAI Platform backend",
    ),
    (
        &["deliberate"],
        Config::cast_can_deliberate,
        "a cast pairing an `explorer` slot with an OFFLINE synth (`lane = \"batch\"` or \
         `lane = \"direct\"`) — see the DELIBERATE casts in docs/config.example.toml",
    ),
    // `generate` runs no reasoning slot at all: it needs the cast's media member. The
    // media CAS gate ([cas] enabled) rides separately in `live_tools` — this rule is
    // only the staffing half.
    (
        &["generate"],
        Config::cast_can_generate,
        "a cast with an `image` slot pointing at a media backend (kind `stability`, \
         `openai-images`, or `dashscope`) — see the image-slot examples in \
         docs/config.example.toml",
    ),
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
/// Which of `usable` can staff each cast-taking tool, keyed by tool name — the one
/// computation behind both the staffing gate (an empty list ⇒ the route is dropped) and
/// the `cast` enum each surviving tool advertises.
///
/// A tool absent from the returned map takes no `cast` at all (`run_kaish`,
/// `list_models`, the collect verbs), so no roster can make it unusable. Shared with
/// `kaibo://config`'s `[runtime]` section so the resource explains the *same* verdict the
/// router acted on, rather than a second implementation that could drift from it.
pub(crate) fn eligible_casts_by_tool(
    config: &Config,
    usable: &[String],
) -> std::collections::HashMap<&'static str, Vec<String>> {
    CAST_ENUM_RULES
        .iter()
        .flat_map(|(tools, is_eligible, _)| {
            let casts: Vec<String> = usable
                .iter()
                .filter(|n| is_eligible(config, n))
                .cloned()
                .collect();
            tools.iter().map(move |&t| (t, casts.clone()))
        })
        .collect()
}

/// The plain-language cast shape tool `name` needs, from its `CAST_ENUM_RULES` entry.
/// `None` for a tool that takes no cast. The operator-facing half of the staffing gate:
/// both the startup warning and `kaibo://config` explain a missing tool with this text.
pub(crate) fn cast_requirement_for(name: &str) -> Option<&'static str> {
    CAST_ENUM_RULES
        .iter()
        .find(|(tools, _, _)| tools.contains(&name))
        .map(|(_, _, requirement)| *requirement)
}

/// Every `#[tool]` route name kaibo can advertise — the fixed universe [`live_tools`]
/// filters. `KaiboHandler::new` asserts each really exists on the router, so a renamed
/// tool method fails the build rather than leaving a gate quietly inert.
pub(crate) const ALL_TOOL_NAMES: [&str; 15] = [
    "consult",
    "consult_submit",
    "explore",
    "deliberate",
    "oneshot",
    "run_kaish",
    "batch_submit",
    "generate",
    "read_cas",
    "write_cas",
    "job_get",
    "job_cancel",
    "job_list",
    "job_wait",
    "list_models",
];

/// Tools that only *collect* or *retrieve* what other tools produce. They are a real
/// surface, but they are not on their own a reason for a server to exist: a kaibo whose
/// entire offering is "fetch a handle" or "read a digest" cannot investigate, answer, or
/// generate anything, which is the useless-server state startup already refuses.
///
/// The `job_*` verbs enforce this through their own liveness (they need a producer).
/// `read_cas` and `write_cas` cannot: they key on the media CAS, which is ON by default,
/// so counting them would make the empty-surface guard in `main` unreachable for every
/// stock install — a check that can no longer fire is a check that no longer protects
/// anything. A kaibo whose entire offering is "hold the bytes I hand you and give them
/// back" cannot investigate, answer, or generate anything either.
pub(crate) const FOLLOWER_TOOL_NAMES: &[&str] = &[
    "read_cas",
    "write_cas",
    "job_get",
    "job_cancel",
    "job_list",
    "job_wait",
];

/// Which tools this config actually advertises: the operator's `--no-<tool>` flags AND
/// a cast that can staff each one.
///
/// The single source for that decision. `KaiboHandler::new` filters the router through
/// it, and `kaibo://config` reports it — including from the CLI, which renders the
/// resource with no router in existence. Deriving the answer twice is how the resource
/// would come to describe a surface the server doesn't actually serve, so it is derived
/// once, here.
///
/// Two tools don't take a `cast` and so can't be unstaffable: `run_kaish` (the shell, no
/// model at all) and `list_models` (an operator query). The four collect verbs are a
/// third shape — castless too, but they exist only to collect handles, so they follow
/// their *producers*: live while any of `consult_submit`, `batch_submit`, or
/// `deliberate` is live, dropped once none is.
pub(crate) fn live_tools(config: &Config, usable: &[String]) -> Vec<&'static str> {
    let eligible = eligible_casts_by_tool(config, usable);
    let can_staff = |name: &str| eligible.get(name).is_none_or(|c| !c.is_empty());
    let gating = &config.tools;

    let consult_live = gating.consult && can_staff("consult");
    let explore_live = gating.explore && can_staff("explore");
    let deliberate_live = gating.deliberate && can_staff("deliberate");
    let oneshot_live = gating.oneshot && can_staff("oneshot");
    let batch_live = gating.batch && can_staff("batch_submit");
    // `generate` needs its flag, a cast with an `image` slot, AND the media CAS on —
    // an artifact-producing tool with nowhere to put artifacts is not advertised
    // ([cas] enabled = false is the operator's explicit choice; the startup log and
    // kaibo://config's [cas] section name it, distinct from the unstaffable warning).
    let generate_live = gating.generate && can_staff("generate") && config.cas.enabled;
    // `read_cas` is the retrieval half of the artifact contract, keyed on the SAME
    // liveness the resource it replaces was keyed on: a live media CAS, nothing else. It
    // takes no cast (there is no model in a byte range) and it is deliberately NOT tied to
    // `[artifacts] enabled` — that flag gates whether the model team may *write*, while
    // `generate`'s artifacts need retrieving whether or not a consult may save.
    let read_cas_live = config.cas.enabled;
    // `write_cas` is the deposit half of the same contract and keys on the same one
    // fact. No `--no-write-cas` flag, matching `read_cas`: `[cas] enabled = false` is
    // already the operator's switch for this store, and a second way to say the same
    // thing is a second thing to keep in agreement.
    let write_cas_live = config.cas.enabled;
    let jobs_live = consult_live || batch_live || deliberate_live || generate_live;

    [
        (consult_live, "consult"),
        // `consult_submit` is the async sibling of `consult` — same capability, a
        // submit/collect surface rather than a blocking one — so it shares the `consult`
        // flag and the same interactive-cast requirement. (A dedicated flag per shape
        // is a plausible split if anyone ever needs only one of the two gated alone.)
        (consult_live, "consult_submit"),
        // The single-phase explorer sweep — its own gate, and the broadest cast rule
        // (any cast with an explorer, interactive or not).
        (explore_live, "explore"),
        (deliberate_live, "deliberate"),
        (oneshot_live, "oneshot"),
        // No cast, no model: the read-only shell is always staffable.
        (gating.run_kaish, "run_kaish"),
        (batch_live, "batch_submit"),
        // Media generation through the cast's image slot; a deferred operation mints a
        // `job-N`, so it is a job producer like the three above.
        (generate_live, "generate"),
        // Client-side retrieval by digest. Operator surface, like the resource it
        // replaces: the inner model team never carries it.
        (read_cas_live, "read_cas"),
        // Client-side deposit by content. Operator surface for the same reason
        // `read_cas` is: the caller is the operator's proxy, and the inner model team
        // never carries either half.
        (write_cas_live, "write_cas"),
        // The collect verbs follow their producers — see the fn doc. "Live" folds the
        // flag AND staffing together, so a producer nothing can staff keeps them no more
        // than a producer the operator switched off does: neither mints a handle.
        (jobs_live, "job_get"),
        (jobs_live, "job_cancel"),
        (jobs_live, "job_list"),
        (jobs_live, "job_wait"),
        // Read-only model discovery — an operator/config query, no cast in sight.
        (gating.list_models, "list_models"),
    ]
    .into_iter()
    .filter_map(|(live, name)| live.then_some(name))
    .collect()
}

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
        Self::new_with_env(config, |k| std::env::var(k).ok())
    }

    /// [`new`](Self::new) with the credential lookup injected — the seam that makes the
    /// staffing gate testable.
    ///
    /// Which casts are *usable* depends on which keys resolve, and the built-in cast
    /// registry merges under every config, so a test that built a handler from a fixture
    /// TOML was really testing "my fixture **plus** whatever keys happen to be in the
    /// developer's environment." That passes on CI and fails on the maintainer's laptop
    /// (or the reverse) — the flakiest kind of test, and it hid real behavior here: the
    /// gate correctly dropped `deliberate` everywhere, but `explore`/`batch_submit`
    /// looked ungated because ambient keys made a built-in cast usable. Tests pass a
    /// closure that answers for exactly the keys the fixture means to have; production
    /// passes the real environment. Deliberately a closure rather than `set_var`, which
    /// is unsafe and process-global — the same reasoning `stability.rs` and
    /// `config_resource.rs` already record.
    pub fn new_with_env(config: Config, get_env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let gating = config.tools;
        // `#[tool_router]` gathers every #[tool] method at compile time; gating is a
        // runtime choice, so build the full router and drop the disabled routes by
        // name. (The methods stay compiled — no dead code — they're just not
        // advertised or callable.)
        let mut tool_router = Self::tool_router();

        // The live cast roster, resolved once here (the same startup moment the handshake
        // resolves its list; a reconnect re-reads it). It feeds BOTH the staffing gate
        // just below and the `cast` enum injection further down.
        let usable: Vec<String> = config
            .usable_casts(&get_env)
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        // Per-tool eligible casts, straight from `CAST_ENUM_RULES`.
        let eligible = eligible_casts_by_tool(&config, &usable);
        // A tool nothing can staff is not advertised. `inject_cast_enum` documents the
        // near-miss this closes: faced with an empty roster it *skipped* injecting the
        // enum (an empty `enum` reads as "no valid value" to a strict client), so the tool
        // shipped looking usable while every call would fail its own gate — exactly the
        // `deliberate`-on-a-stock-install bug. Dropping the route is the honest answer:
        // the model never sees a tool it cannot use, and an unusable tool stops billing
        // resident tokens against every session. Tools take no `cast` are unaffected.

        // Vanishing is right for the model's tool list and wrong for operator
        // discoverability — so say so, once per rule, naming the cast shape that would
        // bring the tool back. Only for tools the operator left ON: a `--no-<tool>` drop
        // is already the operator's own choice and needs no explanation.
        for (tools, _, requirement) in CAST_ENUM_RULES {
            if !eligible.get(tools[0]).is_some_and(Vec::is_empty) {
                continue;
            }
            let on: Vec<&str> = tools
                .iter()
                .copied()
                .filter(|t| gating.enabled(t))
                .collect();
            if on.is_empty() {
                continue;
            }
            tracing::warn!(
                tools = on.join(", "),
                "disabled: no configured cast can staff it — needs {requirement}. \
                 Configured casts are listed at kaibo://config."
            );
        }

        // `generate` has a third gate beyond flag + staffing: the media CAS. When the
        // operator's `[cas] enabled = false` is what holds it back, say that — it is a
        // different answer than "nothing can staff it", and the [cas] section of
        // kaibo://config carries the same fact for a live reader.
        if gating.generate && !config.cas.enabled {
            tracing::warn!(
                tools = "generate",
                "disabled: the media CAS is off ([cas] enabled = false) — an \
                 artifact-producing tool needs somewhere to store artifacts. Remove the \
                 flag and reconnect to bring it back."
            );
        }

        // Which tools actually survive: the operator's flags AND a cast that can staff
        // each. `live_tools` is the single source — `kaibo://config` reports the same
        // decision, so the resource can never describe a surface this router doesn't serve.
        let live = live_tools(&config, &usable);
        // `remove_route` silently no-ops on an unknown name, so a renamed #[tool] method
        // would leave its gate quietly inert. Assert the route exists before dropping it —
        // a stale name is a build-time bug we want loud.
        for name in ALL_TOOL_NAMES {
            if live.contains(&name) {
                continue;
            }
            assert!(
                tool_router.has_route(name),
                "gating: no tool route named {name:?} — did a #[tool] method get renamed?"
            );
            tool_router.remove_route(name);
        }

        // Stamp the live cast roster onto the consultation tools' `cast` param as a
        // JSON-Schema `enum`, so an agent choosing a team reads the menu off the tool
        // schema it already fills arguments from — not only the handshake prose, which
        // a host may truncate (the failure that motivated this).
        //
        // Same `eligible` map the staffing gate above used — a cast's shape (synth lane +
        // explorer) decides which tools it serves, and each rule is an independent filter
        // (the batch and deliberate views OVERLAP — a deliberate-shaped batch cast like
        // `fable` serves both). Routing every enum through this table (and cross-checking
        // it against the gates in the consistency test) is what keeps the advertised menu
        // and the call-time gate from drifting apart. The `cast` enum is the one
        // authoritative per-lane roster — the resident-prose roster this used to also
        // append (`append_cast_roster`) was dropped as redundant.
        //
        // Every empty roster here now belongs to a route the gate already removed, so
        // `inject_cast_enum`'s empty-list skip is a belt-and-braces guard rather than the
        // load-bearing behavior it used to be.
        for (tools, _, _) in CAST_ENUM_RULES {
            let casts = eligible.get(tools[0]).expect("every rule seeded the map");
            inject_cast_enum(&mut tool_router, tools, casts);
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
            let mut meta = MetaObject::new();
            meta.insert(
                "anthropic/alwaysLoad".to_string(),
                serde_json::Value::Bool(true),
            );
            route.attr.meta = Some(meta);
        }

        // Sessions start in-memory. When persistence is enabled, `main` opens the durable
        // store (async, fallible) and swaps it in via `with_session_store` — keeping `new`
        // sync and its many call sites unchanged. A test that never injects a store runs the
        // historical in-memory path.
        let sessions = Sessions::Memory(SessionStore::new(config.defaults.session_capacity));
        // Async-consult jobs get their own cap: a held job result (answer + optional
        // report) is heavier than a session's lean Q&A pair, so `job_capacity` is a
        // separate, smaller knob (`[defaults] job_capacity` / `KAIBO_JOB_CAPACITY`).
        let jobs = JobStore::new(config.defaults.job_capacity);
        // Wrap the config once and hand a clone to the resolver — the single point that
        // computes the canonicalized allowed set and inferred default root, so the CLI
        // front door bounds its own invocation cwd by the exact same rule (see
        // `Resolver::from_config`). The handler keeps its own `Arc<Config>` clone (the
        // same allocation) for its many `self.config` uses.
        let config = Arc::new(config);
        let resolver = Resolver::from_config(config.clone())?;
        // The media CAS starts in-memory when enabled — the same posture sessions take
        // (memory until `main` proves durable state is really available). `main` calls
        // `finalize_media_store` to upgrade to disk / warn / keep it off; a handler
        // built for a test that never finalizes holds a working in-memory store.
        let media_cas = config.cas.enabled.then(|| {
            Arc::new(crate::cas::MediaStore::Memory(crate::cas::MemoryCas::new(
                config.cas.max_bytes,
            )))
        });
        Ok(Self {
            config,
            media_cas,
            backing_probe: crate::cas::probe_backing,
            cas_ephemeral_fs: None,
            tool_router,
            tool_schemas: Arc::new(builtin_schemas()?),
            sessions,
            jobs,
            // An unwired default — nothing pushes to it until `main` swaps in the shared
            // ring the bridge layer feeds (see `with_notifications`). So a handler built
            // for a test has a valid, empty buffer and `job_wait` simply drains nothing.
            notifications: crate::mcp_log::NotificationBuffer::new(512),
            mcp_log_level: Arc::new(AtomicU8::new(mcp_log::rank(mcp_log::DEFAULT_LEVEL))),
            resolver,
            // The real network-client builders; tests swap in a scripted double.
            batch_providers: Arc::new(crate::batch::LiveBatchProviders),
            media_arms: Arc::new(crate::media::LiveMediaArms),
        })
    }

    /// The shared resolver behind this handler — the CLI front door borrows the same
    /// resolution glue from a config without standing up a full handler. Exposed so a
    /// caller that already has a handler (tests, diagnostics) can reach it directly.
    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// Swap in the shared notification ring `main` also handed the bridge layer, so the
    /// `job_wait` tool drains the records the layer pushes. A builder (not a `new` param) to
    /// keep `new(config)` unchanged for the many call sites and tests.
    pub fn with_notifications(mut self, buffer: crate::mcp_log::NotificationBuffer) -> Self {
        self.notifications = buffer;
        self
    }

    /// Swap the in-memory sessions for the durable turso-backed store — the persistence
    /// seam. `main` opens the store (async, fallible, containment-checked against the
    /// resolved allowed set) and calls this when `[persistence]` is enabled; a builder, like
    /// [`with_notifications`](Self::with_notifications), so `new(config)` stays sync and
    /// unchanged. The same store backs both `consult` sessions and batch-handle
    /// recording/recovery (via [`Sessions::store`]).
    pub fn with_session_store(mut self, store: crate::store::SessionStore) -> Self {
        self.sessions = Sessions::Persistent(store);
        self
    }

    /// Settle the media CAS into its final mode, once `main` knows whether persistence
    /// actually came up — the runtime half of [`Config::cas_mode`]'s derivation
    /// (Amy's lifecycle ruling: on and disk-backed while persistence is; on but
    /// in-memory when it's not; `[cas] enabled = false` turns it off).
    ///
    /// - **Disk**: open the store at the fixed XDG data dir (never a model-suppliable
    ///   path), containment-checked against the resolved allowed set. An open failure is
    ///   a LOUD startup error — crash over a silent fallback to memory, mirroring the
    ///   session store's posture.
    /// - **Memory**: keep the in-memory store `new` seeded, and warn SEVERELY: the
    ///   operator is paying provider credits for artifacts that will not survive a
    ///   restart, and must hear that at startup, not discover it after one.
    /// - **Off**: the operator's explicit choice; say so once at info, nothing to build.
    pub fn finalize_media_store(mut self, persistence_active: bool) -> Result<Self> {
        let allowed = self.allowed_set();
        let allowed_refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
        // The three-state decision itself lives in `cas::open_media_store`, shared with the
        // CLI front door — see that function for why one copy. This wrapper is the
        // handler's half: hold the store, and remember an ephemeral finding for
        // `kaibo://config` to report.
        let (store, ephemeral) = crate::cas::open_media_store(
            &self.config.cas,
            self.config.cas_mode(persistence_active),
            &allowed_refs,
            self.backing_probe,
        )?;
        self.media_cas = store.map(Arc::new);
        self.cas_ephemeral_fs = ephemeral;
        Ok(self)
    }

    /// The mode the handler's media store is *actually* in right now — what the
    /// `kaibo://config` render reports. Derived from the live field, not re-derived
    /// from config, so the resource can never describe a store the handler doesn't hold.
    fn live_cas_mode(&self) -> crate::config::CasMode {
        match self.media_cas.as_deref() {
            None => crate::config::CasMode::Off,
            Some(crate::cas::MediaStore::Disk(_)) => crate::config::CasMode::Disk,
            Some(crate::cas::MediaStore::Memory(_)) => crate::config::CasMode::Memory,
        }
    }

    /// The live media store, if the CAS is enabled — for tests and diagnostics.
    pub fn media_store(&self) -> Option<&Arc<crate::cas::MediaStore>> {
        self.media_cas.as_ref()
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

    /// Swap in the backing-filesystem probe — the seam that lets a test say what the CAS
    /// directory is sitting on, since the real answer depends on where the test process
    /// happens to be running. A builder, like
    /// [`with_media_arms`](Self::with_media_arms); apply it BEFORE
    /// [`finalize_media_store`](Self::finalize_media_store), which is what reads it.
    #[cfg(test)]
    pub fn with_backing_probe(mut self, probe: crate::cas::BackingProbe) -> Self {
        self.backing_probe = probe;
        self
    }

    /// The ephemeral filesystem the CAS is sitting on, if the startup probe found one.
    /// `None` covers durable, unknown, and every non-disk mode.
    pub fn cas_ephemeral_fs(&self) -> Option<&'static str> {
        self.cas_ephemeral_fs
    }

    /// Swap in a media-arm factory — the seam that lets tests drive the `generate`
    /// lane (sync store-and-answer, the deferred job, and `read_cas` over what it
    /// stored) with a scripted [`crate::media::MediaModel`] instead of a real provider
    /// client. A builder, like [`with_batch_providers`](Self::with_batch_providers).
    #[cfg(test)]
    pub fn with_media_arms(mut self, arms: Arc<dyn crate::media::MediaArmFactory>) -> Self {
        self.media_arms = arms;
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

    /// Does this server offer anything beyond collecting and retrieving? See
    /// [`FOLLOWER_TOOL_NAMES`] — `main`'s empty-surface guard asks this rather than
    /// "is the list empty", because a surface of nothing but followers is the same
    /// useless server by a different route.
    pub fn has_substantive_tools(&self) -> bool {
        self.advertised_tools()
            .iter()
            .any(|name| !FOLLOWER_TOOL_NAMES.contains(&name.as_str()))
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
        self.resolver.allowed_set()
    }

    /// The effective default root — what a call resolves to when it omits `path`.
    /// The explicit `--root`/config value, or the launch cwd when it was inferred
    /// (see [`Self::default_root_inferred`]); `None` when neither applies. Exposed
    /// for tests and the `kaibo://config` resource.
    pub fn default_root(&self) -> Option<PathBuf> {
        self.resolver.default_root()
    }

    /// Whether [`Self::default_root`] was inferred from the launch cwd rather than
    /// configured explicitly.
    pub fn default_root_inferred(&self) -> bool {
        self.resolver.default_root_inferred()
    }

    /// Shim over [`Resolver::resolve_consult_attachments`] (static — the sandbox
    /// rides in as a param, so the resolution glue is shared with the CLI front door).
    async fn resolve_consult_attachments(
        root: &std::path::Path,
        attach: &[String],
        budget: usize,
        sandbox: &crate::sandbox::SandboxConfig,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        Resolver::resolve_consult_attachments(root, attach, budget, sandbox).await
    }

    /// Shim over [`Resolver::resolve_sweep_attachments`] — the resolution glue lives on
    /// the shared resolver so the CLI `explore` front door runs the same sweep-attach
    /// resolution + image refusal.
    async fn resolve_sweep_attachments(
        &self,
        root: &std::path::Path,
        attach: &[String],
        tool: &str,
    ) -> Result<Vec<crate::consult::ConsultAttachment>, McpError> {
        self.resolver
            .resolve_sweep_attachments(root, attach, tool)
            .await
    }

    /// Shim over [`Resolver::gate_consult_image_attachments`] — the resolution glue
    /// lives on the shared resolver so the CLI runs the same gate.
    fn gate_consult_image_attachments(
        attachments: &[crate::consult::ConsultAttachment],
        vision: bool,
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        Resolver::gate_consult_image_attachments(attachments, vision, model, cast)
    }

    /// Shim over [`Resolver::gate_image_attachments`].
    pub fn gate_image_attachments(
        &self,
        vision: bool,
        attachments: &[crate::attach::Attachment],
        model: &str,
        cast: &str,
    ) -> Result<(), McpError> {
        self.resolver
            .gate_image_attachments(vision, attachments, model, cast)
    }

    /// Shim over [`Resolver::followed_worktrees`].
    fn followed_worktrees(&self) -> Vec<PathBuf> {
        self.resolver.followed_worktrees()
    }

    /// Shim over [`Resolver::house_rules`].
    fn house_rules(&self, root: &std::path::Path) -> Result<Option<Arc<str>>, McpError> {
        self.resolver.house_rules(root)
    }

    /// Shim over [`Resolver::orientation`].
    async fn orientation(&self, root: &std::path::Path) -> Result<Option<Arc<str>>, McpError> {
        self.resolver.orientation(root).await
    }

    /// Shim over [`Resolver::resolved_prompts`].
    fn resolved_prompts(&self, cast: &Cast) -> PromptOverrides {
        self.resolver.resolved_prompts(cast)
    }

    /// Shim over [`Resolver::resolve_cast`].
    fn resolve_cast(&self, cast: Option<String>) -> Result<Cast, McpError> {
        self.resolver.resolve_cast(cast)
    }

    /// Shim over [`Resolver::reject_offline_cast`].
    fn reject_offline_cast(&self, cast: &Cast, tool: &str) -> Result<(), McpError> {
        self.resolver.reject_offline_cast(cast, tool)
    }

    /// Shim over [`Resolver::require_batch_cast`] — the resolution glue lives on the
    /// shared resolver so the CLI `batch submit` front door runs the same gate.
    fn require_batch_cast(&self, cast: &Cast) -> Result<(), McpError> {
        self.resolver.require_batch_cast(cast)
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

    /// Shim over [`Resolver::override_model`]. Only the offline model-override tests
    /// still reach it directly (the live callers go through `apply_model_override`),
    /// so it's test-scoped to stay dead-code-clean in a normal build.
    #[cfg(test)]
    fn override_model(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: &str,
        backend: Option<&str>,
    ) -> Result<(), McpError> {
        self.resolver.override_model(cast, role, model, backend)
    }

    /// Shim over [`Resolver::apply_model_override`].
    fn apply_model_override(
        &self,
        cast: &mut Cast,
        role: ModelRole,
        model: Option<&str>,
        backend: Option<&str>,
        model_arg: &str,
        backend_arg: &str,
    ) -> Result<(), McpError> {
        self.resolver
            .apply_model_override(cast, role, model, backend, model_arg, backend_arg)
    }

    /// Shim over [`Resolver::arm`].
    fn arm(&self, cast: &Cast, role: ModelRole) -> Result<Arm, McpError> {
        self.resolver.arm(cast, role)
    }

    /// Shim over [`Resolver::resolve_root`] — the containment check both front doors
    /// share (see `containment.rs`).
    pub(crate) fn resolve_root(&self, path: Option<String>) -> Result<PathBuf, McpError> {
        self.resolver.resolve_root(path)
    }

    /// Build this call's [`ArtifactSink`], or refuse loudly.
    ///
    /// The two-key gate, resolved in one place so `consult` and `consult_submit` cannot
    /// drift: the operator's standing permission (`[artifacts] enabled`), the caller's
    /// per-call `save_artifacts`, and a live media CAS. All three, or no sink — and with
    /// no sink there is no `save_artifact` in the driver's toolset at all.
    ///
    /// A caller that asked and cannot be served gets an **error**, never a quiet consult
    /// with no artifacts in it. The request is explicit, so a silently-dropped capability
    /// would leave the caller reading an answer that swallowed the bulk it asked to have
    /// stored, with nothing saying why. The message names which key is missing and where
    /// to turn it on.
    fn artifact_sink(
        &self,
        asked: bool,
        question: &str,
        session: Option<&str>,
        cast: &str,
        synth_model: &str,
    ) -> Result<Option<Arc<crate::artifact::ArtifactSink>>, McpError> {
        if !asked {
            return Ok(None);
        }
        if !self.config.artifacts.enabled {
            return Err(McpError::invalid_params(
                "`save_artifacts` was requested, but this kaibo does not allow the model \
                 team to save artifacts. An operator enables it with `[artifacts] enabled \
                 = true` in config.toml, `KAIBO_ARTIFACTS_ENABLED=1`, or \
                 `--allow-save-artifact`, then reconnects. Re-run without \
                 `save_artifacts` to get the answer inline."
                    .to_string(),
                None,
            ));
        }
        let Some(store) = self.media_cas.clone() else {
            return Err(McpError::invalid_params(
                "`save_artifacts` was requested, but the media CAS is off ([cas] enabled \
                 = false), so a saved artifact would have nowhere to land. Re-enable the \
                 CAS and reconnect, or re-run without `save_artifacts`."
                    .to_string(),
                None,
            ));
        };
        Ok(Some(Arc::new(crate::artifact::ArtifactSink::new(
            store,
            crate::artifact::ArtifactAuthor {
                prompt: question.to_string(),
                model: synth_model.to_string(),
                cast: cast.to_string(),
                // Only the consult DRIVER loop carries the tool, and that loop runs on
                // the synth arm. A sweep's sub-agent cannot reach a sink (see
                // `ConsultConfig::artifacts`), so there is no other slot to record.
                slot: "synth",
                session: session.map(str::to_string),
            },
        ))))
    }

    /// Shim over [`Resolver::resolve_attachments`].
    pub async fn resolve_attachments(
        &self,
        paths: &[String],
    ) -> Result<Vec<crate::attach::Attachment>, McpError> {
        self.resolver.resolve_attachments(paths).await
    }

    #[tool(
        description = "Ask a model outside your own family about a codebase — code review, \
            debugging, architecture, \"what does this change break\" — and get a grounded \
            answer with `file:line` citations. A synthesis agent (DeepSeek, Gemini, \
            Anthropic, OpenRouter, or local — pick with `cast`) drives a READ-ONLY shell over \
            the project: it reads the real, current source, delegates broad sweeps to a fast \
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
        meta: RequestMetaObject,
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
                max_attachments: defaults.max_attachments,
            },
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            attachments,
            artifacts: self.artifact_sink(
                input.save_artifacts,
                &input.question,
                input.session_id.as_deref(),
                &cast.name,
                &synth.model,
            )?,
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
            // Artifacts saved before the failure are named in the failure text too — they
            // are durable either way, and a result without their digests orphans them.
            Err(e) => {
                return Ok(consultation_failed_with_artifacts(
                    "consult",
                    &cast.name,
                    e,
                    cfg.artifacts.as_deref(),
                ))
            }
        };
        progress.emit(PhaseEvent::PhaseFinished { phase: "consult" });

        // Provenance: name the cast and the models that answered, so a caller (a
        // cross-model study especially) sees which model produced this without
        // digging into `kaibo://config`. consult runs two arms — both are named.
        // Fold any non-fatal warnings (a failed session record) back into the answer
        // text before the footer — the MCP client has no structured warnings channel, so
        // it sees them inline exactly as #76 shipped (the CLI keeps them off `--json`).
        let answer = consult_answer_text(
            out.answer,
            &out.warnings,
            cfg.artifacts.as_deref(),
            &cast.name,
            &[("explorer", &explorer.model), ("synth", &synth.model)],
            &out.usage,
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
                max_attachments: defaults.max_attachments,
            },
            synth_max_turns: input.synth_max_turns.unwrap_or(defaults.synth_max_turns),
            attachments,
            // Same two-key gate as the sync lane, and refused here for the same reason —
            // synchronously, before a job exists, so a caller asking for something this
            // server cannot do gets a clean error rather than a handle that comes back
            // missing the artifacts it asked for.
            artifacts: self.artifact_sink(
                input.save_artifacts,
                &input.question,
                input.session_id.as_deref(),
                &cast.name,
                &synth.model,
            )?,
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
                    let answer = consult_answer_text(
                        out.answer,
                        &out.warnings,
                        cfg.artifacts.as_deref(),
                        &cast_name,
                        &[
                            ("explorer", explorer_model.as_str()),
                            ("synth", synth_model.as_str()),
                        ],
                        &out.usage,
                    );
                    Ok(JobResult {
                        answer,
                        report: include_report.then_some(out.report),
                    })
                }
                // Render the failure to its final text here (classification + guidance),
                // so `job_get` wraps a ready string without re-deriving anything — with
                // any artifacts this job saved before failing named in it, since they are
                // durable whether or not the answer arrived.
                Err(e) => Err(consultation_failure_text_with_artifacts(
                    "consult",
                    &cast_name,
                    e,
                    cfg.artifacts.as_deref(),
                )),
            }
        });

        let msg = format!(
            "Submitted consultation `{job_id}` on cast `{}`. It runs in the \
             background — go do other work and `job_get {job_id}` for the answer; \
             `job_cancel {job_id}` stops it. Nothing to wait on now.",
            cast.name
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
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
        meta: RequestMetaObject,
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
            max_attachments: defaults.max_attachments,
        };

        let span =
            tracing::info_span!("explore", cast = %cast.name, explorer_model = %explorer.model);
        progress.emit(PhaseEvent::PhaseStarted { phase: "explore" });
        // The top-level `explore` tool doesn't inject `attach` (v1 scope) — its
        // report goes straight back to the calling agent's own context, which is
        // exactly the channel `attach` exists to bypass; no consumer to route to.
        let (report, usage) =
            match explore_with(&input.question, root, &explorer, &cfg, &attachments, None)
                .instrument(span)
                .await
            {
                Ok(out) => out,
                // A provider/model-loop failure is a clean tool-result error, same as `consult`.
                Err(e) => return Ok(consultation_failed("explore", &cast.name, e)),
            };
        progress.emit(PhaseEvent::PhaseFinished { phase: "explore" });

        // The report IS the text (no structured_content). Provenance names the one arm
        // that produced it, so a cross-model study sees which explorer surveyed.
        let report = with_provenance(report, &cast.name, &[("explorer", &explorer.model)], &usage);
        Ok(CallToolResult::success(vec![ContentBlock::text(report)]))
    }

    #[tool(
        description = "Put a top model's deepest reasoning on your codebase without holding \
            a session open. A fast model first investigates the project READ-ONLY and \
            assembles a cited dossier (you wait for this — minutes); a heavyweight synth \
            then deliberates offline over that evidence — a frontier model on the \
            provider's batch lane (max thinking, half price) or a big local model taking \
            the time it takes. Returns a durable handle once the dossier is built; keep \
            working, then `job_wait`/`job_get` it. Best for hard questions worth hours — a \
            design review, a gnarly bug, \"is this abstraction right\". kaibo keeps the \
            dossier and hands back its digest: pass it as `dossier` to put the same \
            evidence in front of a second cast for the price of one synth. For an answer \
            this turn, use `consult`."
    )]
    async fn deliberate(
        &self,
        Parameters(input): Parameters<DeliberateInput>,
        peer: Peer<RoleServer>,
        meta: RequestMetaObject,
    ) -> Result<CallToolResult, McpError> {
        self.deliberate_call(input, progress_sink(peer, &meta))
            .await
    }

    /// The `deliberate` handler, with the live peer already reduced to a progress sink.
    ///
    /// Split from the tool entry point for one reason: a `Peer` needs a real MCP
    /// connection, so the whole handler was unreachable from a test. Everything the reuse
    /// road does — the inert-argument refusal, resolution order, the load, the hand-off to
    /// a lane — is in here, where a test can drive it with a `NullSink` (DeepSeek
    /// cross-family review, 2026-08-07).
    async fn deliberate_call(
        &self,
        input: DeliberateInput,
        progress: Arc<dyn ProgressSink>,
    ) -> Result<CallToolResult, McpError> {
        // A reuse call that also carries explorer arguments is self-contradictory; say so
        // before resolving anything, so it costs nothing.
        if let Some(refusal) = dossier::inert_explorer_args(dossier::ExplorerArgs {
            dossier: input.dossier.as_deref(),
            attach: &input.attach,
            model: input.explorer_model.as_deref(),
            backend: input.explorer_backend.as_deref(),
            max_turns: input.explorer_max_turns,
        }) {
            return Err(McpError::invalid_params(refusal, None));
        }
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
        // Stage 2's preamble, resolved before the branch because both roads reach it:
        // the offline-synth prompt, overridable via `[prompts].batch` OR the synth slot's
        // own `preamble` (`resolved_prompts` layers both, same as the dossier phase does).
        let system =
            crate::consult::batch_system_prompt(self.resolved_prompts(&cast).batch.as_deref());

        // Stage 1 — build the dossier, or reuse one already built.
        //
        // The reuse road exists because a dossier is the expensive half: a sweep can run
        // hundreds of thousands of tokens, and asking a second cast the same question over
        // the same evidence should cost only the second synth. It is also the honest way to
        // compare two synths — same evidence, so the answers differ by the model alone.
        let (dossier, images, dossier_usage, kept, explorer_model) = if let Some(reference) =
            input.dossier.as_deref()
        {
            let (text, kept) = dossier::load_dossier(self.media_cas.as_ref(), reference)
                .map_err(|msg| McpError::invalid_params(msg, None))?;
            // No sweep, so no routed images and no explorer spend — and no explorer to
            // name in the provenance footer. The synth is the only model this call ran.
            (text, Vec::new(), Usage::new(), Some(kept), None)
        } else {
            let explorer = self.arm(&cast, ModelRole::Explorer)?;
            let explorer_model = explorer.model.clone();

            // The dossier is built synchronously, on the live progress sink: the caller
            // waits through this bounded (minutes) explorer sweep, exactly as `explore`
            // does, so a thin/failed dossier is a clean error *before* any offline tokens
            // are spent. Only the deliberation (Stage 2) is handed off async. Attachments
            // reach the dossier-builder as read-WHOLE directives (the sweep semantics), so
            // their content flows to the offline synth through the dossier it writes.
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
                max_attachments: defaults.max_attachments,
            };
            // The offline synth's resolved caps, without building a network client — pure
            // and key-free, so this can run before the (bounded but real) dossier sweep,
            // and it's valid for both lanes (batch and direct both require the synth
            // slot). Lets the dossier sweep's `attach` gate on the synth's real vision
            // cap instead of assuming blind.
            let (synth_slot, _synth_backend, synth_caps) = self.batch_synth(&cast)?;
            let consumer = SweepConsumer {
                kind: SweepConsumerKind::OfflineSynth,
                label: Arc::from(format!(
                    "the offline synth (`{}`) on cast `{}`",
                    synth_slot.id, cast.name
                )),
                vision: synth_caps.vision,
            };
            // Deliberate's dossier sweep does NOT dedupe against the caller's own attach
            // list (an empty seed): a caller-attached file reaches the offline synth ONLY
            // through what the explorer writes (it's a read-WHOLE directive here, never
            // inlined), so deduping would silently strand it — see SweepAttachSink's doc.
            let sink = (defaults.max_attachments > 0).then(|| {
                Arc::new(SweepAttachSink::new(
                    defaults.max_attachments,
                    consumer.clone(),
                    std::collections::HashSet::new(),
                ))
            });

            let span = tracing::info_span!("deliberate.dossier", cast = %cast.name, explorer_model = %explorer_model);
            progress.emit(PhaseEvent::PhaseStarted {
                phase: "deliberate.dossier",
            });
            let (mut dossier, dossier_usage) = match explore_with(
                &input.question,
                root,
                &explorer,
                &cfg,
                &attachments,
                sink.as_ref(),
            )
            .instrument(span)
            .await
            {
                Ok(out) => out,
                Err(e) => return Ok(consultation_failed("deliberate", &cast.name, e)),
            };
            progress.emit(PhaseEvent::PhaseFinished {
                phase: "deliberate.dossier",
            });
            // Stitch whatever the sweep routed via `attach` into the dossier text itself
            // (text bodies, notes, demotions), then keep the finished dossier — that
            // order, so what is stored is what stage 2 receives. Routed images come back
            // separately: they ride the synth's single turn as native parts, never as
            // dossier text. Keeping is never fatal — see `server::dossier`.
            let (images, kept) = stitch_and_keep(
                &mut dossier,
                &consumer,
                sink.as_ref(),
                self.media_cas.as_ref(),
                &input.question,
                &cast.name,
                &explorer_model,
            );
            (dossier, images, dossier_usage, kept, Some(explorer_model))
        };

        // Stage 2 — hand the dossier to the offline synth. Its lane picks the mechanism
        // and the handle.
        match lane {
            Lane::Batch => {
                self.deliberate_batch(
                    &cast,
                    explorer_model.as_deref(),
                    &input.question,
                    &dossier,
                    &images,
                    &system,
                    dossier_usage,
                    kept.as_ref(),
                )
                .await
            }
            Lane::Direct => self.deliberate_direct_job(
                &cast,
                explorer_model.as_deref(),
                &input.question,
                &dossier,
                &images,
                &system,
                dossier_usage,
                kept.as_ref(),
            ),
        }
    }

    /// Stage 2, batch lane: submit the dossier+question as a one-item provider batch
    /// (max thinking, half price) and hand back the durable `backend/provider-id` handle.
    /// The dossier phase already ran, so this is only the submit — reusing the same
    /// `batch::submitter` + shaping `batch_submit` uses, minus the vision gate (a
    /// deliberate caller `attach` reaches the dossier stage as read-whole directives,
    /// so THAT never carries a submit-time attachment part; the dossier is text in
    /// the item prompt). `images` here are different — anything the dossier SWEEP
    /// routed via its own `attach` tool call, already vision-gated on the synth's
    /// caps when the sink was built, so every image handed to `submit` is one this
    /// synth can actually see.
    #[allow(clippy::too_many_arguments)] // each arg is a distinct, named stage-2 input
    async fn deliberate_batch(
        &self,
        cast: &Cast,
        // The explorer that built the dossier, or `None` when this call reused a stored
        // one — there was no explorer then, and naming one would bill a model that never
        // ran to this call.
        explorer_model: Option<&str>,
        question: &str,
        dossier: &str,
        // Images the dossier sweep routed via `attach` — the batch builders already
        // carry images natively (Anthropic/Gemini/OpenAI Responses), so this is a
        // real submit-time attachment, not dossier text.
        images: &[crate::attach::Attachment],
        system: &str,
        // The dossier stage's explorer tokens — real synchronous spend kaibo already
        // paid to build the dossier. The offline synth's own cost lands later on the
        // provider's batch result (not rendered here), but the caller should still see
        // what the build cost rather than have it silently dropped on this lane.
        dossier_usage: Usage,
        // Where the dossier landed in the media CAS, when it was kept. This lane is the
        // long one — a batch deliberation runs for hours and is collected by a later
        // `job_get`, possibly after a restart — so the address rides both the ack and the
        // persisted handle's label, and survives the server session either way.
        kept: Option<&KeptDossier>,
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
            .submit(system, images, &items)
            .instrument(span)
            .await
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let handle = format!("{backend_name}/{provider_id}");
        // Persist the handle so a restart can re-list and re-address this deliberation,
        // exactly as `batch_submit` does — this lane runs for hours, so a restart in the
        // middle is the ordinary case, not the edge one. The label carries the synth model
        // AND the dossier's address, which is what makes the evidence recoverable from
        // `job_list` alone after the ack has scrolled out of the caller's context. A store
        // failure is logged, never fatal: the batch is already live at the provider, so
        // failing here would strand it.
        if let Some(store) = self.sessions.store() {
            let label = match kept {
                Some(k) => format!(
                    "deliberate · synth `{model}` · dossier {}{}",
                    crate::cas::CAS_URI_PREFIX,
                    k.digest
                ),
                None => format!("deliberate · synth `{model}`"),
            };
            if let Err(e) = store
                .put_batch(
                    &backend_name,
                    &provider_id,
                    Some(&label),
                    Some(items.len() as i64),
                )
                .await
            {
                tracing::warn!(handle = %handle, error = %e, "could not persist batch handle");
            }
        }
        // Fold the dossier build's real cost into the ack — the synth's own tokens land
        // later on the provider's batch result, but the build spend is knowable now. A
        // reused dossier cost this call nothing, and says so.
        let stage_one = match explorer_model {
            Some(m) => {
                let cost = fmt_usage(&dossier_usage)
                    .map(|t| format!(", {t}"))
                    .unwrap_or_default();
                format!("Dossier built (explorer `{m}`{cost})")
            }
            None => "Dossier reused (no explorer ran)".to_string(),
        };
        let msg = format!(
            "{stage_one} and handed to the batch lane as `{handle}` — cast `{}`, synth \
             `{model}` at max thinking. It deliberates offline; collect it with `job_get \
             {handle}` (durable — survives restart), or stop it with `job_cancel \
             {handle}`. Nothing to wait on now.{}",
            cast.name,
            dossier_ack(kept)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    /// Stage 2, direct lane: spawn a session-scoped `job-N` that runs the big LOCAL synth
    /// as one long toolless completion over the dossier. No provider handle exists on this
    /// lane, so the job stays `job-N` end to end (said loudly in the reply — a restart
    /// loses it, matching the standing no-daemon decision). Mirrors `consult_submit`'s
    /// spawn, but the background work is `deliberate_direct`, not the consult loop.
    #[allow(clippy::too_many_arguments)] // each arg is a distinct, named stage-2 input
    fn deliberate_direct_job(
        &self,
        cast: &Cast,
        // `None` when the dossier was reused rather than swept for — see the batch lane's
        // note. The provenance footer then names the synth alone, because the synth is the
        // only model this call ran.
        explorer_model: Option<&str>,
        question: &str,
        dossier: &str,
        // Images the dossier sweep routed via `attach` — ride the synth's single
        // turn as native parts (`user_turn_with_attachments`, shared with `oneshot`).
        images: &[crate::attach::Attachment],
        system: &str,
        // The dossier stage's explorer tokens, summed into the final footer with the
        // synth's — the footer names both roles, so it counts both.
        dossier_usage: Usage,
        // Where the dossier landed, when it was kept — named in the ack now and in the
        // finished answer later, so a `job_get` that arrives long after the ack still
        // carries the evidence trail.
        kept: Option<&KeptDossier>,
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
        // `job_get`/`job_list`. The direct lane is a single completion with no tools, so
        // it emits no beats of its own — the log carries the job's own start/finish.
        let progress_log = Arc::new(ProgressLog::new(Arc::new(TracingSink)));
        let cast_name = cast.name.clone();
        let swept = explorer_model.is_some();
        let explorer_model = explorer_model.map(str::to_string);
        let question = question.to_string();
        let dossier = dossier.to_string();
        let images = images.to_vec();
        let system = system.to_string();
        let kept_for_job = kept.cloned();
        let label = format!("cast `{cast_name}` deliberate (direct synth `{synth_model}`)");

        let job_id = self.jobs.submit(label, progress_log, async move {
            match crate::consult::deliberate_direct(
                &question, &dossier, &images, &synth, &system, deadline,
            )
            .await
            {
                Ok((answer, synth_usage)) => {
                    // Name only the models this call actually ran: a reused dossier had no
                    // explorer, and the token line counts the synth alone because that is
                    // the whole spend.
                    let mut roles: Vec<(&str, &str)> = Vec::with_capacity(2);
                    if let Some(m) = explorer_model.as_deref() {
                        roles.push(("explorer", m));
                    }
                    roles.push(("synth", synth_model.as_str()));
                    Ok(JobResult {
                        answer: with_provenance(
                            // The dossier line sits between the answer and the provenance
                            // footer: it is part of what this deliberation is grounded in,
                            // not part of who ran it.
                            with_dossier(answer, kept_for_job.as_ref()),
                            &cast_name,
                            &roles,
                            &(dossier_usage + synth_usage),
                        ),
                        report: None,
                    })
                }
                Err(e) => Err(consultation_failure_text("deliberate", &cast_name, e)),
            }
        });

        let msg = format!(
            "{}; the direct (local) synth is now deliberating offline as `{job_id}` — cast \
             `{}`. This is one long local completion (it can take a while): `job_wait \
             {job_id}` parks for it, `job_get {job_id}` collects, `job_cancel {job_id}` \
             stops it. Session-scoped — the job lives for this server session only.{}",
            if swept {
                "Dossier built"
            } else {
                "Dossier reused (no explorer ran)"
            },
            cast.name,
            dossier_ack(kept)
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
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
        meta: RequestMetaObject,
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
        let (answer, usage) = match oneshot(&input.prompt, &attachments, &arm, &cfg)
            .instrument(span)
            .await
        {
            Ok(out) => out,
            // A provider failure is a clean tool-result error, same as `consult`.
            Err(e) => return Ok(consultation_failed("oneshot", &cast.name, e)),
        };
        progress.emit(PhaseEvent::PhaseFinished { phase: "oneshot" });

        let answer = with_provenance(answer, &cast.name, &[("model", &arm.model)], &usage);
        Ok(CallToolResult::success(vec![ContentBlock::text(answer)]))
    }

    #[tool(
        description = "Run a kaish (sh-like) script against the READ-ONLY project; \
            returns exit code + stdout + stderr. Read generously with line numbers — \
            `cat -n FILE` for a whole file, `grep -rn PATTERN .` to locate across \
            files — and compose builtins with pipes (grep/jq/awk/find/...). Writes are \
            refused (exit 1, stderr `permission denied: filesystem is read-only`) and \
            external commands are unreachable (exit 127); 124 = timed out. \
            Each call starts fresh at the project root. See `kaibo://kaish/*` (or \
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

        Ok(CallToolResult::success(vec![ContentBlock::text(
            format_output(&out),
        )]))
    }

    #[tool(
        description = "List available models a backend's provider actually serves — \
            model discovery, read-only: which models can I use, what's the context \
            window, is a new model out yet. Queries the backend's real /models \
            endpoint (DeepSeek, OpenRouter, Anthropic, Gemini, or any OpenAI-compatible \
            endpoint) with kaibo's already-configured auth, no `curl` + hand-rolled \
            headers needed. Omit `backend` to sweep every configured backend at once. \
            No cast, no model in the loop — a pure operator/config query, like \
            `kaibo://config`."
    )]
    pub async fn list_models(
        &self,
        Parameters(input): Parameters<ListModelsInput>,
    ) -> Result<CallToolResult, McpError> {
        let names: Vec<String> = match input.backend {
            Some(name) => {
                // Confirm the name resolves before spending a network call on it —
                // an unknown backend is a usage error, not a per-backend sweep entry.
                let backend = self
                    .config
                    .resolve_backend(&name)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                vec![backend.name.clone()]
            }
            None => self.config.backends.keys().cloned().collect(),
        };

        let mut results: std::collections::BTreeMap<
            String,
            Result<Vec<crate::discover::DiscoveredModel>, String>,
        > = std::collections::BTreeMap::new();
        for name in names {
            // A backend resolved above always resolves again here (nothing mutates
            // the registry mid-call); skip defensively rather than panic if that
            // invariant ever changes.
            let Ok(backend) = self.config.resolve_backend(&name) else {
                continue;
            };
            let fetcher = crate::discover::HttpModelListFetcher::new(backend.request_timeout);
            let outcome = crate::discover::discover_models(backend, &fetcher)
                .await
                .map_err(|e| format!("{e:#}"));
            results.insert(name, outcome);
        }

        let mut result = CallToolResult::success(vec![ContentBlock::text(
            crate::discover::render_models(&results),
        )]);
        // Mirror the CLI's `--json` face: a machine caller gets the same envelope a
        // human gets as prose, so the two never drift on what a sweep looks like.
        result.structured_content = Some(crate::discover::models_json_envelope(&results));
        Ok(result)
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
            if !crate::batch::batch_supported(b) {
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
            .filter(|b| crate::batch::batch_supported(b))
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
        // Now build the network client (resolves the key). By here `require_batch_cast`
        // has already vouched the cast is batch-capable, so a submitter build failure is
        // an internal config inconsistency (an unbuildable key/endpoint on a lane we just
        // validated), not the caller's parameter mistake — hence `internal_error`.
        let provider = self
            .batch_providers
            .submitter(backend, slot, &self.config.defaults)
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        let items: Vec<crate::batch::BatchItem> = input
            .prompts
            .iter()
            .enumerate()
            .map(|(i, p)| crate::batch::BatchItem {
                custom_id: i.to_string(),
                prompt: p.clone(),
            })
            .collect();
        // Batch is the oneshot *shape* (the synthesis agent answering from what it was
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
        // Persist the handle (label = model) so a restart can re-list and re-address it —
        // the durable memory of what kaibo launched. The provider stays the source of truth
        // for batch STATE; this is bookkeeping. A store failure is logged, never fatal: the
        // batch is already live at the provider, so failing the call would strand it (and
        // `job_list`'s provider query still recovers it). Only when persistence is enabled.
        if let Some(store) = self.sessions.store() {
            if let Err(e) = store
                .put_batch(
                    &backend_name,
                    &provider_id,
                    Some(&model),
                    Some(items.len() as i64),
                )
                .await
            {
                tracing::warn!(handle = %handle, error = %e, "could not persist batch handle");
            }
        }
        let msg = format!(
            "Submitted batch `{handle}` — {} prompt(s) on cast `{}` (model `{}`). \
             Poll it with `job_get {handle}` (it'll show progress, then per-item answers \
             when done); stop it with `job_cancel {handle}`.",
            items.len(),
            cast.name,
            model
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(
        description = "Store an image in kaibo's media store and get back its digest — \
            the deposit half of `read_cas`, and how an image REACHES kaibo. Name the \
            file in `path`: kaibo reads it itself, so the bytes never cost you a token. \
            `content` takes base64 instead, for an image that is not a file; a real \
            screenshot through `content` is megabytes you have to write out, so reach \
            for `path` whenever the image is on disk. Pass exactly one. The result is a \
            kaibo://cas/<digest> address, the mime, the size, and the real file path \
            when the store is on disk. The format is read from the bytes themselves, so \
            there is no mime to pass and none to get wrong: png, jpeg, gif and webp are \
            accepted and anything else is refused. Images are capped at 8388608 bytes \
            and a larger one is refused, not trimmed. `path` must be inside the allowed \
            set, like every other path kaibo reads. Nothing is written to your project \
            — this store is kaibo's own, addressed only by content hash."
    )]
    pub async fn write_cas(
        &self,
        Parameters(input): Parameters<WriteCasInput>,
    ) -> Result<CallToolResult, McpError> {
        // The route is dropped when the CAS is off, so this is a belt for a direct
        // caller that bypasses the advertised list.
        let Some(store) = &self.media_cas else {
            return Err(McpError::invalid_params(
                "the media CAS is disabled ([cas] enabled = false), so there is nowhere \
                 to store an upload. Re-enable it (or remove the flag) and reconnect."
                    .to_string(),
                None,
            ));
        };
        // Exactly one source. Neither is a caller who has not said what to store; both
        // is a caller who said it twice and would need kaibo to pick — a silent
        // precedence rule is how the stored bytes come to disagree with the intent.
        let stored = match (input.path.as_deref(), input.content.as_deref()) {
            (Some(path), None) => {
                let bytes = self
                    .resolver
                    .read_contained_file(path, crate::upload::MAX_UPLOAD_BYTES as u64)
                    .await?;
                crate::upload::store_bytes(
                    store,
                    &bytes,
                    input.label.as_deref(),
                    now_epoch_secs(),
                )
            }
            (None, Some(content)) => crate::upload::store_upload(
                store,
                content,
                input.label.as_deref(),
                now_epoch_secs(),
            ),
            (None, None) => {
                return Err(McpError::invalid_params(
                    "write_cas needs an image: name the file in `path`, or pass its \
                     base64 bytes in `content` when it is not a file."
                        .to_string(),
                    None,
                ))
            }
            (Some(_), Some(_)) => {
                return Err(McpError::invalid_params(
                    "write_cas takes `path` or `content`, not both — kaibo will not \
                     guess which one you meant to store. Pass the one that names this \
                     image."
                        .to_string(),
                    None,
                ))
            }
        }
        // A store I/O failure or a capacity refusal is a server-side condition, not a
        // parameter the caller got wrong — the same split `read_cas` makes between a bad
        // digest and an unreadable object.
        .map_err(|e| match e {
            crate::upload::UploadError::Store(_) => {
                McpError::internal_error(e.to_string(), None)
            }
            _ => McpError::invalid_params(e.to_string(), None),
        })?;

        let hex = stored.digest.to_hex();
        let mut text = format!(
            "Stored 1 image (read it back with `read_cas`, passing the digest):\n\
             {CAS_RES_PREFIX}{hex} ({}, {} bytes)",
            stored.extension.mime(),
            stored.bytes,
        );
        if let Some(path) = store.path_for(&stored.digest) {
            text.push_str(&format!("\n   path: {}", path.display()));
        }
        if stored.provenance_missing {
            // The bytes are durable and addressable; only the record beside them is
            // missing. Said plainly rather than left to be discovered by a later
            // `read_cas` reporting "provenance: absent".
            text.push_str(
                "\n   NOTE: the image is stored and retrievable, but kaibo could not \
                 record the metadata beside it — nothing on disk says what this image is \
                 or when it arrived. The digest above is still good.",
            );
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Read one stored artifact by its digest — what `generate` and a \
            consult's `save_artifact` hand back as kaibo://cas/<digest>. Metadata \
            always comes first: mime, total size, whether it is binary, the artifact's \
            label, the range served, and the real file path when the store is on disk \
            (open that directly with your own tools for anything large). Reads are \
            BOUNDED, and what you get depends on the object. TEXT: omitting `length` \
            returns up to 65536 bytes from `offset`, and `offset` pages the rest — the \
            metadata's total tells you how far. IMAGE: a whole image up to 5 MiB comes \
            back viewable; a larger one comes back as metadata alone (plus the file path \
            in disk mode). ANY BINARY: omitting `length` returns metadata only, never a \
            wall of base64 — pass `length` to get a base64 range. `length` 0 is metadata \
            only for anything. `length` is capped at 1048576 bytes and a larger ask is \
            refused, not trimmed."
    )]
    pub async fn read_cas(
        &self,
        Parameters(input): Parameters<ReadCasInput>,
    ) -> Result<CallToolResult, McpError> {
        // The route is dropped when the CAS is off, so this is a belt for a direct caller
        // that bypasses the advertised list.
        let Some(store) = &self.media_cas else {
            return Err(McpError::invalid_params(
                "the media CAS is disabled ([cas] enabled = false); no artifacts are \
                 held, so there is nothing to read."
                    .to_string(),
                None,
            ));
        };
        // Validated before it can become part of a path — the same structural guard the
        // resource this tool replaces applied, and the reason a traversal-shaped
        // "digest" never reaches a lookup.
        let digest = crate::cas::Digest::from_hex(&input.digest)
            .map_err(|e| McpError::invalid_params(format!("{e}"), None))?;

        // Whole-object read, verified against the digest, THEN sliced. See
        // `cas_read`'s module doc: the store's verify-before-return guarantee is worth
        // more than the local I/O a ranged read would save, and the budget this tool
        // exists to protect is the caller's context, not the disk.
        let (bytes, ext) = match store.get(&digest) {
            Ok(Some(found)) => found,
            Ok(None) => {
                return Err(McpError::invalid_params(
                    format!(
                        "no artifact with digest {} — it was never produced on this \
                         store, or (in memory mode) it did not survive a restart",
                        input.digest
                    ),
                    None,
                ))
            }
            // Corrupt or unreadable stays loud and distinct: an object whose bytes do not
            // hash to their address is not "missing", and folding the two would let real
            // corruption read as an ordinary absence.
            Err(e) => return Err(McpError::internal_error(format!("{e}"), None)),
        };
        // The record and the label are two facts, not one: an object can have a record
        // that carries no label, or no record at all (a sidecar write that failed, or one
        // that went missing — `Cas::entry_for`'s probe fallback still serves the object).
        // The metadata says which.
        let provenance = store.provenance(&digest);
        let label = provenance.as_ref().and_then(|p| p.label.clone());
        let path = store.path_for(&digest);
        let view = cas_read::plan(
            &cas_read::CasObject {
                digest: &input.digest,
                ext,
                bytes: &bytes,
                label: label.as_deref(),
                provenance_present: provenance.is_some(),
                path: path.as_deref(),
            },
            input.offset.unwrap_or(0),
            input.length,
            // An MCP host renders the image block; the CLI, serving bytes to a stream,
            // passes `Bytes` so the metadata never claims a rendering that didn't happen.
            cas_read::Delivery::Rendered,
        )
        .map_err(|e| McpError::invalid_params(e, None))?;

        // Metadata leads every response — a caller that reads only the first block still
        // learns what this object is, how big it is, and where the rest of it lives.
        let mut blocks = vec![ContentBlock::text(view.meta)];
        match view.body {
            cas_read::Body::None => {}
            cas_read::Body::Text(text) => blocks.push(ContentBlock::text(text)),
            cas_read::Body::Base64(data) => blocks.push(ContentBlock::text(data)),
            // The one hop to the eye: hosts render an image content block straight to a
            // vision model, the same mechanism the inner `view_image` tool rides.
            cas_read::Body::Image { data, mime } => blocks.push(ContentBlock::image(data, mime)),
        }
        Ok(CallToolResult::success(blocks))
    }

    #[tool(
        description = "Generate images from a text prompt with the cast's `image` \
            model (a media backend: Stability, an OpenAI-compatible images endpoint — \
            hosted gpt-image or a local stable-diffusion.cpp sd-server — or DashScope's \
            wan family). \
            Bytes are never inlined: each artifact lands in kaibo's content-addressed \
            media store and the result lists per-artifact digests — a \
            kaibo://cas/<digest> address, the mime, the provider's seed when reported, \
            and the real file path when the store is on disk. Fetch one with `read_cas` \
            (metadata first, ranges on request; a small image comes back viewable). \
            Provider-native options (aspect_ratio, size, n, output_format, seed, \
            negative_prompt, style_preset, ...) pass through `fields` verbatim, each \
            value's JSON type (string | number | boolean) preserved to the wire. \
            To generate FROM an image rather than from the prompt alone, pass its \
            digest in `inputs` under the provider's field name — \
            `inputs {\"image\": \"<digest>\"}` with `fields {\"strength\": 0.6}` is \
            image-to-image. Digests come from `write_cas` or an earlier `generate`, so \
            an image already in the store is reused by address and never re-sent. \
            Stability accepts input images; the other media backends do not and say so. \
            `op` picks an operation when the backend has more than one — Stability's \
            edit, control and upscale routes, each with its required `inputs` keys and \
            its credit cost listed on the parameter, so you can see the price before you \
            pick. Omit `op` to generate from the prompt. An \
            operation the provider declares deferred returns a `job-N` handle for \
            job_wait/job_get instead (every route wired today answers in-call). \
            Provenance (prompt, model, cast, seed) is recorded beside every artifact."
    )]
    pub async fn generate(
        &self,
        Parameters(input): Parameters<GenerateInput>,
    ) -> Result<CallToolResult, McpError> {
        // The route is dropped when the CAS is off, so this arm is a belt for a direct
        // caller that bypasses the advertised list.
        let Some(store) = self.media_cas.clone() else {
            return Err(McpError::invalid_params(
                "the media CAS is disabled ([cas] enabled = false), so `generate` has \
                 nowhere to store artifacts. Re-enable it (or remove the flag) and \
                 reconnect."
                    .to_string(),
                None,
            ));
        };
        let cast = self.resolve_cast(input.cast)?;
        let Some(slot) = cast.slot(ModelRole::Image) else {
            return Err(McpError::invalid_params(
                format!(
                    "cast `{}` has no `image` slot — `generate` needs a cast whose \
                     `image` slot points at a media backend (kind {}). kaibo://config \
                     lists the configured casts and their slots.",
                    cast.name,
                    crate::credentials::media_kinds_list(),
                ),
                None,
            ));
        };
        // Slot backend refs are resolved at config load, so a miss here is kaibo's
        // inconsistency, not the caller's parameter mistake.
        let backend = self
            .config
            .resolve_backend(&slot.backend)
            .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
        // Building the arm resolves the provider key — a missing key is the operator's
        // setup gap, reported cleanly with the cast and slot named.
        let arm = self.media_arms.build(backend, slot).map_err(|e| {
            McpError::invalid_params(format!("cast `{}` image slot: {e:#}", cast.name), None)
        })?;
        let fields: Vec<(String, crate::media::FieldValue)> = input
            .fields
            .unwrap_or_default()
            .into_iter()
            .map(|(name, value)| (name, value.into_field_value()))
            .collect();
        // Reserved keys: recorded provenance must describe the request that actually
        // ran. `fields.prompt` would send prompt B while the sidecar records prompt A,
        // and `fields.model` would reroute the call (Stability's SD3 route, the
        // Images API's model field) while the sidecar records the slot's model — so
        // both are refused loudly, pointing at the real parameter.
        for (key, param) in [
            ("prompt", "the `prompt` parameter"),
            (
                "model",
                "the cast's `image` slot (its model id picks the provider model/route)",
            ),
        ] {
            if fields.iter().any(|(name, _)| name == key) {
                return Err(McpError::invalid_params(
                    format!(
                        "`fields.{key}` is reserved — it would make the recorded \
                         provenance disagree with the request that ran. Set it through \
                         {param} instead."
                    ),
                    None,
                ));
            }
        }
        // Resolved before the arm is called, so a bad digest refuses without spending a
        // provider request — and the store, not the caller, names each part's format.
        let inputs = match input.inputs.as_ref() {
            Some(asked) => crate::media::resolve_inputs(&store, asked)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?,
            None => Vec::new(),
        };
        let request = crate::media::MediaRequest {
            prompt: input.prompt.clone(),
            fields,
            inputs,
            op: input.op.clone(),
        };
        let span = tracing::info_span!("generate", cast = %cast.name, model = %arm.slot_ref());
        match arm.generate(&request).instrument(span).await {
            Ok(crate::media::MediaOutcome::Complete(artifacts)) => {
                let rendered = match store_generated_artifacts(
                    &store,
                    &artifacts,
                    &input.prompt,
                    arm.slot_ref(),
                    &cast.name,
                ) {
                    Ok(text) => text,
                    Err(e) => return Ok(consultation_failed("generate", &cast.name, e)),
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    with_provenance(
                        rendered,
                        &cast.name,
                        &[("image", arm.slot_ref())],
                        &Usage::new(),
                    ),
                )]))
            }
            // The provider declared this operation deferred: hand back a `job-N` on the
            // existing collect verbs, with a background task owning the poll cadence.
            // The lane split follows the operation's DECLARED shape (see
            // stability::Operation::shape), never a sniffed response.
            Ok(crate::media::MediaOutcome::Deferred(provider_job)) => {
                let progress_log = Arc::new(ProgressLog::new(Arc::new(TracingSink)));
                let label = format!("generate · cast {} · image {}", cast.name, arm.slot_ref());
                let prompt = input.prompt.clone();
                let cast_name = cast.name.clone();
                let slot_ref = arm.slot_ref().to_string();
                let deadline = self.config.defaults.call_deadline;
                let poll_arm = arm.clone();
                let id = self.jobs.submit(label, progress_log, async move {
                    let started = tokio::time::Instant::now();
                    loop {
                        match poll_arm.poll(&provider_job).await {
                            Ok(crate::media::MediaPollOutcome::Complete(artifacts)) => {
                                let text = store_generated_artifacts(
                                    &store, &artifacts, &prompt, &slot_ref, &cast_name,
                                )
                                .map_err(|e| {
                                    consultation_failure_text("generate", &cast_name, e)
                                })?;
                                return Ok(JobResult {
                                    answer: with_provenance(
                                        text,
                                        &cast_name,
                                        &[("image", &slot_ref)],
                                        &Usage::new(),
                                    ),
                                    report: None,
                                });
                            }
                            Ok(crate::media::MediaPollOutcome::Pending) => {
                                if started.elapsed() > deadline {
                                    return Err(format!(
                                        "still pending after {}s (the call_deadline \
                                         budget) — provider job id `{}` may still \
                                         finish on the provider's side, but kaibo has \
                                         stopped polling it",
                                        deadline.as_secs(),
                                        provider_job.0
                                    ));
                                }
                                tokio::time::sleep(GENERATE_POLL_INTERVAL).await;
                            }
                            Err(e) => {
                                return Err(consultation_failure_text("generate", &cast_name, e))
                            }
                        }
                    }
                });
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Deferred: `{id}` — the provider is generating in the background \
                     (cast `{}`, image `{}`). Collect it with `job_get {id}` or park on \
                     `job_wait`; stop it with `job_cancel {id}`.",
                    cast.name,
                    arm.slot_ref()
                ))]))
            }
            Err(e) => Ok(consultation_failed("generate", &cast.name, e)),
        }
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
            // The submitted count this handle was recorded with, when persistence is on
            // and the record has one — a store miss (persistence off, an unknown handle,
            // or a lookup error) is `None`, the same "no cross-check to run" gap a legacy
            // handle already renders as. A lookup failure is logged, never fatal: the
            // provider is still the source of truth for the poll itself.
            let submitted = match self.sessions.store() {
                Some(store) => match store.get_batch(backend_name, provider_id).await {
                    Ok(handle) => handle.and_then(|h| h.submitted_count).map(|n| n as u64),
                    Err(e) => {
                        tracing::warn!(handle = %input.handle, error = %e, "could not read the batch handle's submitted count");
                        None
                    }
                },
                None => None,
            };
            let span = tracing::info_span!("job_get", handle = %input.handle);
            let poll = provider
                .poll(provider_id, submitted)
                .instrument(span)
                .await
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
            let label = format!("{backend_name} · {provider_id}");
            return Ok(CallToolResult::success(vec![ContentBlock::text(
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
                    "no background job `{}` — it may have finished and been evicted by \
                     newer submits, been canceled, or never existed. Job ids look \
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
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Requested cancellation of batch `{}`. `job_get` it for the final \
                 per-item results.",
                input.handle
            ))]));
        }
        self.ensure_consult_enabled(&input.handle)?;
        // Producer-neutral wording throughout: a `job-N` may be a consultation, a
        // deliberation, or a deferred generation — the handle doesn't say which.
        let msg = match self.jobs.cancel(&input.handle) {
            CancelOutcome::Canceled => format!("Canceled job `{}`.", input.handle),
            CancelOutcome::AlreadyFinished => format!(
                "Job `{}` had already finished — `job_get` it for the result.",
                input.handle
            ),
            CancelOutcome::Unknown => {
                return Err(McpError::invalid_params(
                    format!(
                        "no background job `{}` to cancel — it may have finished and \
                         been evicted, or never existed.",
                        input.handle
                    ),
                    None,
                ));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
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

        // In-memory jobs first — this session. `consult_submit`, `deliberate`'s direct
        // lane, and deferred `generate` all land here, so the section shows whenever any
        // producer is live — the same predicate the `job-N` collect guard uses
        // (`job_producer_live`), so a server never accepts handles it won't list.
        if self.job_producer_live() {
            sections.push(render_jobs_section(&self.jobs.list()));
        }

        // Handles the live provider listing surfaced — so the recovered-handles section
        // below (from the persistence store) shows only what the live list *didn't*,
        // never a duplicate.
        let mut shown_handles: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                                    shown_handles.insert(handle.clone());
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

        // Recovered batch handles — kaibo's own durable record of batches it submitted,
        // from the persistence store. Surfaces handles the live listing above didn't return
        // (an unqueried backend, or an orphan from a past server session after a restart),
        // so a restart never loses the way back to a batch. Deduped against the live list;
        // the provider stays the source of truth for STATE — `job_get` a recovered handle
        // for its live status. Only present when persistence is enabled.
        if let Some(store) = self.sessions.store() {
            match store.list_batches().await {
                Ok(handles) => {
                    let lines: Vec<String> = handles
                        .iter()
                        .map(|h| {
                            (
                                format!("{}/{}", h.backend, h.provider_id),
                                h.label.as_deref(),
                            )
                        })
                        .filter(|(handle, _)| !shown_handles.contains(handle))
                        .map(|(handle, label)| match label {
                            Some(l) => format!("- `{handle}` — {l}"),
                            None => format!("- `{handle}`"),
                        })
                        .collect();
                    if !lines.is_empty() {
                        sections.push(format!(
                            "Recovered batch handles (kaibo-submitted, from the persistence \
                             store — `job_get` one for live status):\n{}",
                            lines.join("\n")
                        ));
                    }
                }
                Err(e) => sections.push(format!("Recovered batch handles: unavailable — {e}")),
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            sections.join("\n\n"),
        )]))
    }

    #[tool(description = "Park for async work to make progress: blocks up to \
            `timeout_secs`, returning early only when a job finishes or fails, else on a \
            clean timeout — the productive alternative to polling `job_get`. \
            `level:\"info\"` folds the live narrative (each shell command, sweep, \
            milestone) into the result without changing when it returns; name batch \
            `handles` to fold their status in.")]
    async fn job_wait(
        &self,
        Parameters(input): Parameters<WaitInput>,
        peer: Peer<RoleServer>,
        meta: RequestMetaObject,
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
        // `level` sizes the *observability sample* — how much narrative rides back in the
        // result — never *when* the call returns. Wake is always the Warn bar (a job
        // finished/failed, or a real mid-flight warning); narrative below it rides along in
        // the tail but never cuts the park short, so `level:"info"` parks-and-coalesces
        // instead of returning on the first `running kaish: …` line. Cap the sample floor at
        // Warn so the terminal ping is always in the tail even if a caller asks higher.
        let wake_floor = crate::mcp_log::rank(LoggingLevel::Warning);
        let sample_floor = wait_level_floor(input.level.as_deref())?.min(wake_floor);
        let limit = input.limit.unwrap_or(20);
        let timeout = std::time::Duration::from_secs(input.timeout_secs.unwrap_or(60));

        // The live view for the *human*: while this call is open, stream the Info-level
        // narrative (each kaish command, sweep, milestone) as `notifications/progress` on
        // this call's token, so the client renders it in real time — the channel sync
        // `consult` used, reopened on demand. Independent of what we *return* to the model:
        // the human watches the show live, the model gets the coalesced tail at wake/timeout.
        let info_floor = crate::mcp_log::rank(LoggingLevel::Info);
        let token = progress_token(&meta);
        // Drain down to whichever is lower — Info (to stream) when a token is present,
        // else just the sample floor (don't consume narrative no one is watching).
        let drain_floor = if token.is_some() {
            info_floor.min(sample_floor)
        } else {
            sample_floor
        };
        let seq = AtomicU64::new(0);
        let records = self
            .notifications
            .wait_drain_with(
                timeout,
                drain_floor,
                sample_floor,
                wake_floor,
                limit,
                |rec| {
                    // Stream Info+ to the human's progress channel; the model's return is the
                    // separate `sample_floor` tail collected inside `wait_drain_with`.
                    if let Some(token) = &token {
                        if crate::mcp_log::rank(rec.level) >= info_floor {
                            let param = ProgressNotificationParam::new(
                                token.clone(),
                                seq.fetch_add(1, Ordering::Relaxed) as f64,
                            )
                            .with_message(format!(
                                "[{}] {}",
                                wait_level_label(rec.level),
                                rec.message
                            ));
                            let peer = peer.clone();
                            // Fire-and-forget, like `ProgressReporter`: don't make the drain
                            // loop await a notification it doesn't depend on.
                            tokio::spawn(async move {
                                let _ = peer.notify_progress(param).await;
                            });
                        }
                    }
                },
            )
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
                    Ok(provider) => {
                        // Same submitted-count lookup `job_get` does — best-effort, so the
                        // gentle poll here never blocks on a store hiccup.
                        let submitted = match self.sessions.store() {
                            Some(store) => store
                                .get_batch(backend, id)
                                .await
                                .ok()
                                .flatten()
                                .and_then(|handle| handle.submitted_count)
                                .map(|n| n as u64),
                            None => None,
                        };
                        match provider.poll(id, submitted).await {
                            Ok(poll) => format!("{h} — {}", batch_poll_brief(&poll)),
                            Err(e) => format!("{h} — poll failed: {e:#}"),
                        }
                    }
                    Err(e) => format!("{h} — {}", e.message),
                },
                Err(e) => format!("{h} — {}", e.message),
            };
            batch_lines.push(line);
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            render_wait(&records, &batch_lines, &self.jobs, timeout),
        )]))
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

    /// Whether any producer of in-memory `job-N` handles is live on this server:
    /// `consult_submit`, `deliberate`'s direct lane, or a deferred `generate`. The ONE
    /// predicate behind both the `job_list` in-memory section and the `job-N` collect
    /// guard, so the section a server renders and the handles it accepts can't drift.
    /// `generate` is judged by route liveness, not its bare flag: the flag defaults on
    /// but the tool only mints a `job-N` when it is actually advertised (a cast can
    /// staff it AND the media CAS is on), and a stock install has neither — the flag
    /// alone would claim producers on servers where generate can't run.
    fn job_producer_live(&self) -> bool {
        self.config.tools.consult
            || self.config.tools.deliberate
            || self.tool_router.has_route("generate")
    }

    /// Refuse a `job-N` handle only when no tool that produces one is enabled. A `job-N`
    /// comes from `consult_submit`, `deliberate`'s direct lane, OR a deferred `generate`,
    /// so any of the three keeps it collectible.
    fn ensure_consult_enabled(&self, handle: &str) -> Result<(), McpError> {
        if self.job_producer_live() {
            return Ok(());
        }
        Err(McpError::invalid_params(
            format!(
                "`{handle}` looks like a background job (`job-N`), but nothing that \
                 produces one is enabled on this server (--no-consult --no-deliberate \
                 --no-generate)."
            ),
            None,
        ))
    }
}

// `router = self.tool_router` is load-bearing, not decoration. The attribute's DEFAULT
// is `Self::tool_router()` — a fresh router rebuilt from the compile-time `#[tool]` set,
// which knows nothing about the per-instance gating `new_with_env` applied: dropped
// routes, the injected `cast` enums, the `alwaysLoad` pin. Naming the field routes
// `tools/list`, `tools/call`, and `get_tool` through the handler's OWN router, so what
// the wire advertises and accepts is what `advertised_tools` reports. rmcp 0.16
// defaulted to the field and 1.x+ defaults to the constructor, so the 0.16 → 3.0 bump
// silently bypassed every `--no-<tool>` flag and every staffing drop. Only a test that
// speaks MCP can catch that — a handler-side assertion reads the gated router either
// way; see `tests/mcp_stdio.rs`.
/// The SEP-2549 response-caching fields, filled when this session's negotiated
/// protocol version requires them (`2026-07-28` and newer), absent otherwise.
///
/// rmcp negotiates `2026-07-28` whenever a client asks for it (the version is in the
/// SDK's known list), and that spec version makes both fields REQUIRED on every list
/// and read result. A strictly-validating client (Claude Code, observed 2026-08-10
/// against rmcp 3.0.0-beta.5) accepts the negotiated version and then rejects the
/// whole `tools/list` over the missing fields: zero tools, kaibo unusable until
/// restart.
///
/// rmcp 3.1.1 fills the fields for the two results its handler macros generate
/// (`tools/list`, `prompts/list`) and answers `cacheScope: public`. That is not
/// kaibo's answer, and it does not reach kaibo's other results: the resource methods
/// have no fill at all (the default `ServerHandler` bodies return `::default()`), and
/// kaibo writes all of these methods by hand regardless. So kaibo answers for itself,
/// with the no-caching floor stated honestly: `ttlMs: 0` (immediately stale — the
/// advertised surface is resolved per connection from env, config, and staffing, so a
/// cached copy goes wrong exactly when it matters) and `cacheScope: private` (the
/// surface is this user's own configuration; an intermediary must not serve it to
/// another user). Older sessions stay on their legacy wire shape with the fields
/// absent, and the raw-wire tests in `tests/mcp_stdio.rs` pin both sides.
///
/// The alternative — refusing the version instead of fulfilling it — is available
/// since rmcp 3.1.0 (`ServerHandler::supported_protocol_versions`, upstream #1093;
/// on the 3.0.0-beta.5 this was written against, the serve loop overrode any handler
/// answer). Fulfilling stays the right call: a client that asks for the newest
/// version wants what that version's other features buy it, and the two fields cost
/// kaibo nothing to answer truthfully.
fn sep_2549_cache_fields(context: &RequestContext<RoleServer>) -> (Option<u64>, Option<CacheScope>) {
    // ISO `YYYY-MM-DD` versions compare lexically the same as chronologically.
    let required = context
        .protocol_version()
        .is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str());
    if required {
        (Some(0), Some(CacheScope::Private))
    } else {
        (None, None)
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for KaiboHandler {
    /// The `#[tool_handler]` macro's own `list_tools`, with one difference: the
    /// SEP-2549 fields answer `cacheScope: private`, where the macro answers
    /// `public`. Writing the method here is what suppresses the macro's version (it
    /// skips any method the impl already has), and it must keep serving
    /// `self.tool_router` for the reason the comment above states.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let (ttl_ms, cache_scope) = sep_2549_cache_fields(&context);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms,
            cache_scope,
        })
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
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
        )
        // Identify as kaibo, not rmcp (from_build_env reports the rmcp crate).
        .with_server_info(
            Implementation::new("kaibo", env!("CARGO_PKG_VERSION")).with_title("kaibo"),
        )
        .with_protocol_version(ProtocolVersion::LATEST)
        // Judge provider usability from the live environment so a fresh install
        // (no key, no config) gets setup guidance in the handshake. Read once here,
        // at initialize — the same point the rest of config is bound; reconnecting
        // is what re-reads a newly-set key.
        .with_instructions(kaibo_instructions_with_scope(
            &self.config,
            self.resolver.allowed_trees(),
            self.resolver.default_root_ref(),
            self.resolver.default_root_inferred(),
            self.config
                .default_cast_usability(|k| std::env::var(k).ok()),
            &self.config.usable_casts(|k| std::env::var(k).ok()),
        ))
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let (ttl_ms, cache_scope) = sep_2549_cache_fields(&context);
        Ok(ListResourcesResult {
            resources: kaibo_resources(),
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let (ttl_ms, cache_scope) = sep_2549_cache_fields(&context);
        Ok(ListResourceTemplatesResult {
            resource_templates: kaibo_resource_templates(),
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let (ttl_ms, cache_scope) = sep_2549_cache_fields(&context);
        // Compute the runtime-derived worktree set here (it needs the handler's
        // allowed_set and reflects worktrees that exist *now*); the renderer is a
        // pure function of its inputs, so it can't reach back for this itself.
        read_kaibo_resource_with_config(
            &request.uri,
            &self.tool_schemas,
            &self.config,
            self.resolver.allowed_trees(),
            self.resolver.default_root_ref(),
            self.resolver.default_root_inferred(),
            self.followed_worktrees(),
            self.sessions.store().is_some(),
            self.live_cas_mode(),
            self.cas_ephemeral_fs,
        )
        .map(|response| match response {
            // SEP-2549 covers read results too; fill the same session-gated fields.
            ReadResourceResponse::Complete(mut result) => {
                result.ttl_ms = ttl_ms;
                result.cache_scope = cache_scope;
                ReadResourceResponse::Complete(result)
            }
            other => other,
        })
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let (ttl_ms, cache_scope) = sep_2549_cache_fields(&context);
        Ok(ListPromptsResult {
            prompts: kaibo_prompts(),
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        kaibo_prompt_messages(&request.name, request.arguments.as_ref())
            .map(GetPromptResponse::Complete)
    }
}

/// A `text/markdown` resource at `uri` with `name`/`description`. Small helper so
/// the listing reads as a table of what kaibo serves.
fn markdown_resource(uri: &str, name: &str, description: &str) -> rmcp::model::Resource {
    Resource::new(uri, name)
        .with_mime_type("text/markdown")
        .with_description(description)
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
        Resource::new(CONFIG_URI, "kaibo: runtime config")
            .with_mime_type("application/toml")
            .with_description(
                "kaibo's resolved runtime configuration: allowed path trees, default \
                 cast, gated tools, sandbox limits, each backend with its kind and \
                 key sources, and each cast with its resolved slots. Read this to \
                 understand the server's current posture before making calls.",
            ),
        // The annotated config template — every knob, commented, ready to copy to
        // ~/.config/kaibo/config.toml. The setup guidance on a fresh install points here.
        Resource::new(CONFIG_EXAMPLE_URI, "kaibo: config example")
            .with_mime_type("application/toml")
            .with_description(
                "An annotated kaibo config template: every option with its default and a \
                 comment, plus example backends and casts. Copy to \
                 ~/.config/kaibo/config.toml and edit. Pairs with kaibo://config, which \
                 shows the *resolved* runtime state.",
            ),
        // The reference manual behind the other two: what each knob means and why.
        markdown_resource(
            CONFIG_GUIDE_URI,
            "kaibo: configuration guide",
            "The full configuration reference: precedence across call/CLI/env/file, the \
             backend + cast model, tool gating (why a tool may not be advertised), path \
             containment, persistence, telemetry, house rules, and prompt overrides. Read \
             this when kaibo://config/example's comments leave a question open.",
        ),
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
const CONFIGURE_PROMPT_INTRO_MCP: &str = "\
You're configuring **kaibo**, the MCP server you're connected to right now — it lends \
your work a second opinion from models outside your own family. This sets up which \
models it uses.

Work through these steps:

1. Read kaibo's config resources first (they're MCP resources — no tool turn spent):
   • `kaibo://config/example` — the annotated config.toml template, every knob explained.
   • `kaibo://config` — the resolved live state: the casts and backends that exist now, \
and where each key is sourced from.
   • `kaibo://config/guide` — the full reference manual, if a question stays open.
";

/// Steps 2-6, channel-neutral (which provider, what roster shape, keeping secrets out
/// of the file, optional read-scope widening, and host-agent sandbox setup) — the substance both the MCP
/// `configure` prompt and `kaibo configure` (CLI) share verbatim, so a future edit to
/// the roster-design guidance can't drift between the two front doors. Each caller
/// wraps it in its own channel-specific opening (how to *read* kaibo's own config) and
/// closing (how to make kaibo *pick up* the written file).
const CONFIGURE_STEPS_CORE: &str = "\
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
slug where offered. When you pick a synth model, read its output ceiling from kaibo's \
model listing (the `list_models` tool, or `kaibo models` on the CLI) and set that \
slot's `max_tokens` from the ceiling, because reasoning bills into the same completion \
budget as the answer. Some providers publish no ceiling; there, look it up in the \
provider's own model documentation.
4. Keep secrets in the environment or a key file. A backend names an env var \
(`api_key_env`) or a key-file path (`api_key_file`); the TOML carries the name or path, \
the secret stays outside it. Tell me which env vars to set or files to write, and let \
me put the keys in myself.
5. (Optional) Read scope. By default kaibo reads only the project tree (plus linked git \
worktrees) and only ever *reads* it — never writes to your project. To let the team see \
a scratch space — a \
diff, a log, a generated file you dropped somewhere — name that directory in \
`[server] allow_paths` (`$VAR` / `${VAR}` and a leading `~` expand, resolving per machine). \
It's a deliberate opt-in worth asking me about first, since it widens what a consult can \
read (and can ship to a model).
6. Host-agent sandbox. kaibo's own model-facing shell stays read-only, but the host \
agent or MCP client that launches kaibo may sandbox the kaibo process. A useful kaibo \
setup needs outbound network access to the model APIs I configure, and long-lived MCP \
servers need write access to kaibo's own XDG state path \
(`$XDG_STATE_HOME/kaibo/state.db`, else `~/.local/state/kaibo/state.db`). If a \
media-producing tool is enabled, its content-addressed media CAS lives under the XDG \
data dir (`$XDG_DATA_HOME/kaibo/cas`, else `~/.local/share/kaibo/cas`) and may also \
need host-sandbox access. Ask before opening those paths. I may prefer separate \
per-client stores — for example a Codex-only state db or CAS dir — when I don't want \
Claude Code, Codex, and other agents sharing session history or generated artifacts. \
Codex has a stronger sandbox default than Claude Code in common setups, so its config \
often needs explicit `network_access` and `writable_roots`; Claude Code usually starts \
local MCP servers with ordinary access to my home XDG dirs.
";

const CONFIGURE_PROMPT_OUTRO_MCP: &str = "\
7. When the file is written, remind me to reconnect the kaibo MCP server so it re-reads \
the config and keys — both load once at startup.";

/// The CLI-flavored opening/step-1: kaibo's own config surfaces read as plain
/// subcommands, no MCP client needed — the whole reason `kaibo configure` exists is
/// for a caller that may not have MCP resource access at all.
const CONFIGURE_PROMPT_INTRO_CLI: &str = "\
You're configuring **kaibo** — it lends your work a second opinion from models outside \
your own family. This sets up which models it uses.

Work through these steps:

1. Read kaibo's own config surfaces first, no MCP client needed:
   • `kaibo example-config` — the annotated config.toml template, every knob explained.
   • `kaibo config` — the resolved live state: the casts and backends that exist now, \
and where each key is sourced from.
";

const CONFIGURE_PROMPT_OUTRO_CLI: &str = "\
7. When the file is written, you're done — kaibo re-reads `config.toml` fresh on every \
invocation, so the very next `kaibo consult` (or any other subcommand) picks it up. \
Only reconnect something if you're *also* running kaibo as a long-lived MCP server \
elsewhere — that process still loads config once at startup.";

/// The prompts kaibo advertises (`list_prompts`). Currently just `configure`.
fn kaibo_prompts() -> Vec<Prompt> {
    vec![Prompt::new(
        CONFIGURE_PROMPT_NAME,
        Some(
            "Guide your agent through writing a kaibo config.toml: it reads kaibo's own \
             config resources, asks which providers and models you have, and writes the \
             file. Pass an optional `goal` to steer the roster.",
        ),
        Some(vec![PromptArgument::new("goal")
            .with_title("Setup goal")
            .with_description(
                "What you want from the setup, e.g. \"a local-only privacy cast\" or \
                 \"a cheap DeepSeek explorer with a Claude synth\". Optional — omit for a \
                 general walk-through.",
            )
            .with_required(false)]),
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
            // Blank/whitespace-vs-absent is normalized in `append_configure_goal` (the
            // one gate both this MCP path and the CLI's `run_configure` go through), so
            // this just extracts the raw value.
            let goal = arguments
                .and_then(|a| a.get("goal"))
                .and_then(|v| v.as_str());
            Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                Role::User,
                configure_prompt_text(goal),
            )])
            .with_description("Configure kaibo's models for this codebase"))
        }
        other => Err(McpError::invalid_params(
            format!("unknown prompt {other:?}; kaibo offers: {CONFIGURE_PROMPT_NAME}"),
            None,
        )),
    }
}

/// Appends an optional caller `goal` to a rendered configure body — shared by the MCP
/// prompt and the CLI text so the goal-weaving behavior can't diverge between them. A
/// blank/whitespace-only goal reads as "no goal" (trimmed and filtered here, the one
/// gate both callers go through) — a CLI caller's `kaibo configure ""` or `"   "` gets
/// the same clean output as the MCP prompt's blank-goal case, not a dangling
/// "My goal for this setup:" line.
fn append_configure_goal(mut body: String, goal: Option<&str>) -> String {
    if let Some(goal) = goal.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str("\n\nMy goal for this setup: ");
        body.push_str(goal);
    }
    body
}

/// The `configure` MCP prompt body, with an optional caller `goal` appended verbatim.
/// `pub(crate)` so tests reach it directly. The CLI equivalent is
/// [`configure_prompt_text_cli`] — same [`CONFIGURE_STEPS_CORE`] roster-design
/// guidance, wrapped in this channel's opening/closing (MCP resource reads, reconnect
/// the server) rather than the CLI's (plain subcommands, nothing to reconnect).
pub(crate) fn configure_prompt_text(goal: Option<&str>) -> String {
    let body =
        format!("{CONFIGURE_PROMPT_INTRO_MCP}{CONFIGURE_STEPS_CORE}{CONFIGURE_PROMPT_OUTRO_MCP}");
    append_configure_goal(body, goal)
}

/// The `kaibo configure` CLI text — the same [`CONFIGURE_STEPS_CORE`] roster-design
/// guidance as the MCP `configure` prompt (so an edit to the substance can't drift
/// between the two front doors), wrapped in CLI-flavored framing: kaibo's own config
/// surfaces read as plain subcommands (no MCP client needed — the whole reason this
/// command exists), and no "reconnect the server" step, since a one-shot CLI
/// invocation re-reads `config.toml` fresh every time. `pub(crate)` so `cli.rs`'s
/// `kaibo configure` prints it.
pub(crate) fn configure_prompt_text_cli(goal: Option<&str>) -> String {
    let body =
        format!("{CONFIGURE_PROMPT_INTRO_CLI}{CONFIGURE_STEPS_CORE}{CONFIGURE_PROMPT_OUTRO_CLI}");
    append_configure_goal(body, goal)
}

/// The URI templates kaibo advertises: per-builtin help and per-cast prompts, each
/// addressed by name.
fn kaibo_resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
    let builtin = ResourceTemplate::new(BUILTIN_URI_TEMPLATE, "kaish builtin help")
        .with_description(
            "Help for a single kaish builtin — parameters and examples. \
             e.g. kaibo://kaish/builtin/grep",
        )
        .with_mime_type("text/markdown");
    let prompts = ResourceTemplate::new(PROMPTS_CAST_URI_TEMPLATE, "kaibo: one cast's prompts")
        .with_description(
            "The system preamble each phase gets for a specific cast, its per-slot \
             `preamble`s folded in as a live call resolves them. e.g. kaibo://prompts/deepseek",
        )
        .with_mime_type("text/markdown");
    // Artifact retrieval is deliberately NOT here. It was a `kaibo://cas/{digest}`
    // template until 2026-08-05, and it is now the `read_cas` tool: hosts treat resources
    // as ambient context to prefetch or attach, which is the wrong posture for
    // model-authored bytes, and `resources/read` is whole-blob with no way to ask what an
    // object is before pulling all of it. See `cas_read`'s module doc.
    vec![builtin, prompts]
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
- **`explore` — read-whole directives.** Its explorer reads through the shell, so
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

The explorer can attach too, mid-sweep: a `consult`/`deliberate` investigator that finds a
file where the whole thing IS the evidence can route its real bytes straight to whoever
reads its report — the `consult` answer, or a `deliberate` dossier — without transcribing
a span into its own report first. You never call this yourself; it's the explorer's own
tool (`[defaults] max_attachments`, default 32 files per sweep, `0` turns it off).

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

`consult` hands back an *answer* — a synthesis agent investigates and concludes. `explore`
hands back the *evidence*: it's the fast, cheap explorer half of `consult`, run on its
own, so you get the structured cited report — a summary of findings, the relevant
`file:line` locations, and the trail the explorer followed — with no synthesis on top.
Reach for `explore` to map unfamiliar code, or to assemble a grounded survey you'll reason
over yourself (or hand to another model). It reads the repo itself, like `consult`, so the
same `path` / `cast` / `explorer_model` / `explorer_backend` arguments apply, plus `attach`
(text files the explorer is ordered to read whole during the sweep); no `context` or
`session_id` — those belong to the tools that carry a synthesis agent. Since it runs
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
  work, then call `job_wait` to block (up to `timeout_secs` — your choice, to a 3600s
  ceiling). It parks the whole window and returns early only when a job finishes or fails
  (a real event) — narrative alone never cuts it short — then hands back a sample of what
  happened. By default that sample is what kaibo flagged for you (the milestones); pass
  `level: \"info\"` to fold in the watchable narrative too — each kaish command, sweep, and
  milestone the agents ran. `level` sizes the sample, never the timing; to check in more
  often, pass a shorter `timeout_secs`, not a higher level.

This is a fire-and-forget lane. Submit, then go do other work — don't sit in a tight
poll/sleep loop holding your turn open. `job_wait` when you're ready to spend a minute;
`job_get`/`job_list` are the source of truth. Nothing here wakes you, and nothing is lost
by waiting — the handles keep.

## Generate media: `generate`

`generate` turns a text prompt into images through the cast's `image` slot — a media
backend: Stability's v2beta family, an OpenAI-compatible images endpoint (hosted
gpt-image, or a local stable-diffusion.cpp sd-server), or DashScope's wan family; one
call may return several images via `n`. It is advertised only when a configured cast carries that slot and
the media CAS is on. The result is never inline bytes: each artifact lands in kaibo's
content-addressed store and you get its digest as a `kaibo://cas/<digest>` address, the
mime, the provider's seed when reported, and — when the store is on disk — the real file
path. Provider-native
options ride the `fields` object verbatim, each value's JSON type preserved
(Stability: `aspect_ratio` \"16:9\", `output_format` png|jpeg|webp, `seed`,
`negative_prompt`, `style_preset`; OpenAI-compatible: `size` \"1024x1024\", `n`,
`quality`, `output_format` png|jpeg|webp; DashScope: `size` \"1024*1024\", `n` 1-4,
`seed`, `negative_prompt`). An operation the provider declares
deferred hands back a `job-N` on the same collect verbs above — the lane is wired,
though every route wired today answers in-call. Every artifact gets a provenance
sidecar (prompt, model, cast, timestamp, mime, seed) beside it in the store.

## Getting an image in: `write_cas`

`write_cas` is the deposit half, and how an image reaches kaibo at all. Pass the raw bytes
base64-encoded in `content`; you get back a `kaibo://cas/<digest>` address, the mime, the
size, and the real file path when the store is on disk.

There is no `mime` parameter and no path in either direction. The format is read out of the
bytes themselves — png, jpeg, gif and webp are accepted, anything else is refused — so
there is no claim to get wrong, and the address is the content hash, so there is nothing to
aim. `content` is capped at 8388608 bytes before encoding, and a larger upload is refused
rather than trimmed. An optional `label` records one short line about the image, which
`read_cas` reports back.

Nothing here writes to your project. This is kaibo's own store, and the inner model team
neither carries this tool nor knows the store exists.

## Reading artifacts back: `read_cas`

`read_cas` is the retrieval half — for you, the client, never for kaibo's own models.
Pass the digest from a `kaibo://cas/<digest>` address and you get **metadata first**:
mime, total bytes, whether it is binary, the artifact's label when its record carries one,
the range served, and the real file path when the store is on disk.

Reads are bounded, and the default depends on what the object is. Text: omitting `length`
gives up to 65536 bytes from `offset`, and `offset` pages the rest — the metadata's total
says how far. An image up to 5 MiB with no range asked for arrives whole and viewable; a
larger one arrives as metadata alone, plus the file path when the store is on disk. Any
other binary gives metadata only until you ask: pass `length` for a base64 range, capped
at 1048576 bytes (a larger ask is refused rather than trimmed). `length: 0` is the cheap
look at any object.

Paging always advances. A window that lands inside a multi-byte character comes back as
base64 of exactly the bytes you asked for, with a note saying so — so the served range
still moves and you can widen `length` or realign `offset`, rather than reading the same
byte forever.

## Driving the read-only shell (`run_kaish`)

`run_kaish` runs a kaish (sh-like) script against the project and returns exit code +
stdout + stderr. Lead with the idioms that produce accurate `file:line`s: `cat -n FILE`
to read a file WHOLE (the default; most files fit in one read),
`grep -rn PATTERN .` to find which files matter. A whole read that truncates (exit 3)
still returns the start and end of the file; read the rest as targeted wide spans
(`grep -n SYMBOL FILE`, then `cat -n FILE | sed -n '1200,2400p'`). Compose builtins with pipes
(`grep`/`jq`/`awk`/`find`/…). Each call starts fresh at the project root.

A few habits from `bash` that *won't* carry over here — reach for the kaish form instead:

- `$VAR` is **one word**, always — kaish never splits it on whitespace. When you actually
  want to split, use the `split` builtin; that's the deliberate form.
- Adjacent tokens don't paste together — quote to join: `\"$dir/file.txt\"`, not
  `$dir/file.txt`.
- This shell is **read-only**: a write or a redirect that would create a file is refused
  with exit `1` and the message `permission denied: filesystem is read-only`; an external
  command is unreachable and exits `127`. That's the boundary working, not a bug — read
  freely, and don't try to mutate. Refusals and ordinary failures share exit `1`, so read
  the stderr line when you need to tell them apart.

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
    if uri == CONFIG_GUIDE_URI {
        // Static and config-independent — the embedded manual verbatim.
        return Some(CONFIG_GUIDE_MD.to_string());
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

/// How often a deferred `generate` re-polls its provider. The provider owns no cadence
/// (`MediaModel::poll` is one shot by contract); the waiting side does, bounded overall by
/// `[defaults] call_deadline`. Shared with the CLI's foreground poll so one knob describes
/// both — a caller waiting at a terminal and a background job are the same request.
pub(crate) const GENERATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Store every artifact of one completed generation in the media store and render the
/// per-artifact result lines: `kaibo://cas/<digest>`, the mime, the provider seed when
/// reported, and the real file path in disk mode. One provenance sidecar per artifact
/// (each has its own digest — the one-to-many consequence of
/// [`crate::media::MediaOutcome::Complete`] carrying a list). An empty list is refused:
/// a provider reporting success with nothing attached is that provider's bug, surfaced
/// loudly rather than rendered as an empty success.
///
/// The artifacts are paid for, so partial failure is handled in two layers (or-gpt
/// review, 2026-08-03). First, the WHOLE list is prevalidated — every mime must map
/// onto the store's closed on-disk set — before the first byte is written, so a list
/// with one unstorable member stores *nothing* instead of storing some and then
/// erroring with their digests discarded. Second, a store failure that can only occur
/// mid-loop (I/O, the soft cap) returns an error that NAMES the digests already
/// stored: they exist, they were paid for, and they stay retrievable by those
/// addresses — an error that hid them would orphan them.
pub(crate) fn store_generated_artifacts(
    store: &crate::cas::MediaStore,
    artifacts: &[crate::media::MediaArtifact],
    prompt: &str,
    model: &str,
    cast: &str,
) -> Result<String> {
    use anyhow::{anyhow, ensure};
    ensure!(
        !artifacts.is_empty(),
        "the provider reported a completed generation with zero artifacts"
    );
    // Prevalidate every mime up front — an unknown format is refused rather than
    // written under an invented extension (the same argument
    // stability::MediaType::to_cas_extension records at length), and refusing BEFORE
    // the first put is what keeps a mixed list all-or-nothing at this layer.
    let exts: Vec<crate::cas::Extension> = artifacts
        .iter()
        .enumerate()
        .map(|(i, artifact)| {
            // `is_image` and not merely "the store can name it": the store also names
            // the text formats `save_artifact` writes, and an images provider handing
            // back a text body is a provider fault, not an artifact. Keeping the
            // refusal keyed to the media lane's own shape is what stopped growing
            // `Extension` from quietly widening what `generate` accepts.
            crate::cas::Extension::from_mime(&artifact.mime)
                .filter(crate::cas::Extension::is_image)
                .ok_or_else(|| {
                    anyhow!(
                        "artifact {} has mime {:?}, which is not an image format the \
                         media store can name on disk — refusing the whole result \
                         rather than storing it under an invented extension; nothing \
                         was stored",
                        i + 1,
                        artifact.mime
                    )
                })
        })
        .collect::<Result<_>>()?;
    let timestamp = now_epoch_secs();
    let mut lines = vec![format!(
        "Generated {} artifact{} (read one with `read_cas`, passing the digest):",
        artifacts.len(),
        if artifacts.len() == 1 { "" } else { "s" }
    )];
    let mut stored: Vec<String> = Vec::new();
    for (i, (artifact, ext)) in artifacts.iter().zip(exts).enumerate() {
        let provenance = crate::cas::Provenance {
            prompt: prompt.to_string(),
            model: model.to_string(),
            cast: cast.to_string(),
            timestamp,
            mime: artifact.mime.clone(),
            seed: artifact.seed.clone(),
            // A provider rendered these bytes; no model on kaibo's team authored them,
            // so the authorship fields stay absent and the sidecar keeps the shape it
            // has always had apart from naming its producer.
            tool: Some("generate".to_string()),
            slot: None,
            label: None,
            session: None,
        };
        let digest =
            store
                .put(&artifact.bytes, ext, &provenance)
                .map_err(|e| match stored.is_empty() {
                    true => anyhow!("storing artifact {}: {e} — nothing was stored", i + 1),
                    false => anyhow!(
                        "storing artifact {} of {}: {e}. The artifact{} stored before the \
                     failure {} paid for and retrievable: {}",
                        i + 1,
                        artifacts.len(),
                        if stored.len() == 1 { "" } else { "s" },
                        if stored.len() == 1 {
                            "remains"
                        } else {
                            "remain"
                        },
                        stored
                            .iter()
                            .map(|hex| format!("{CAS_RES_PREFIX}{hex}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                })?;
        let hex = digest.to_hex();
        let mut line = format!("{}. {CAS_RES_PREFIX}{hex} ({}", i + 1, artifact.mime);
        if let Some(seed) = &artifact.seed {
            line.push_str(&format!(", seed {seed}"));
        }
        line.push(')');
        if let Some(path) = store.path_for(&digest) {
            line.push_str(&format!("\n   path: {}", path.display()));
        }
        lines.push(line);
        stored.push(hex);
    }
    Ok(lines.join("\n"))
}

/// Read one kaibo resource by URI, with the runtime config and allowed set threaded
/// in for `kaibo://config`. The pure path (kaish/*, sandbox) routes through
/// `render_resource` (line below); the config arm renders via `render_config_resource`.
///
/// This is the handler-level dispatch: call it from `read_resource` so the config
/// resource gets its config.
// The resolved-config inputs the config arm needs are inherently many (allowed set,
// default root + inferred flag, live worktrees, persistence-active) — bundling them into
// a struct would just relocate the arg list, so we accept the count here.
#[allow(clippy::too_many_arguments)]
fn read_kaibo_resource_with_config(
    uri: &str,
    schemas: &[ToolSchema],
    config: &Config,
    allowed_set: &[PathBuf],
    default_root: Option<&Path>,
    default_root_inferred: bool,
    followed_worktrees: Vec<PathBuf>,
    persistence_active: bool,
    cas_mode: crate::config::CasMode,
    cas_ephemeral_fs: Option<&'static str>,
) -> Result<ReadResourceResponse, McpError> {
    if uri == PROMPTS_URI {
        return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(
                render_prompts_resource(config, None),
                uri,
            )],
        )));
    }
    if let Some(name) = uri.strip_prefix(PROMPTS_CAST_PREFIX) {
        // `kaibo://prompts/<cast>` — the cast's resolved framing (name or alias). An
        // unknown cast is a not-found whose message already names the known casts, so a
        // caller sees the real roster, not a bare miss.
        let cast = config
            .resolve_cast(name)
            .map_err(|e| McpError::resource_not_found(format!("{e:#} (in {uri})"), None))?;
        return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(
                render_prompts_resource(config, Some(cast)),
                uri,
            )],
        )));
    }
    if uri == CONFIG_URI {
        let body = render_config_resource(
            config,
            allowed_set,
            default_root,
            default_root_inferred,
            followed_worktrees,
            persistence_active,
            cas_mode,
            cas_ephemeral_fs,
        );
        return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(body, uri)],
        )));
    }
    if uri == CONFIG_EXAMPLE_URI {
        // Static, config-independent — the embedded template verbatim.
        return Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(CONFIG_EXAMPLE_TOML, uri)],
        )));
    }
    // A host that cached the old resource template still asks for this. The route is
    // gone and stays gone — but a bare "unknown resource" tells a caller nothing about
    // where its artifacts went, and the digest it already holds is the whole argument it
    // needs. Recognized, never served: the answer is a pointer to `read_cas`.
    if let Some(hex) = uri.strip_prefix(crate::cas::CAS_URI_PREFIX) {
        return Err(McpError::resource_not_found(
            format!(
                "the {}<digest> resource was removed — artifact retrieval is now the \
                 `read_cas` tool. Call read_cas with digest {hex} (it takes an optional \
                 `offset`/`length`, and answers metadata first).",
                crate::cas::CAS_URI_PREFIX
            ),
            None,
        ));
    }
    let body = render_resource(uri, schemas)
        .ok_or_else(|| McpError::resource_not_found(format!("unknown resource: {uri}"), None))?;
    Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
        vec![ResourceContents::text(body, uri)],
    )))
}

/// The MCP token the client attached for progress, if any. Per the spec, progress
/// notifications are sent *only* when the client opted in by putting a
/// `progressToken` in the request `_meta`; absent one, we stay silent. Pure so the
/// opt-in/opt-out decision is testable without a live request.
fn progress_token(meta: &RequestMetaObject) -> Option<ProgressToken> {
    meta.get_progress_token()
}

/// Render one [`PhaseEvent`] as an MCP progress notification under `token`. `seq` is
/// the monotonically increasing `progress` value the spec requires (it "should
/// increase every time progress is made, even if the total is unknown"); `total`
/// stays `None` because a consult's step count isn't known up front. Pure — the
/// counting and wiring live in [`ProgressReporter`]; this is just the shape.
fn progress_param(token: ProgressToken, seq: u64, event: &PhaseEvent) -> ProgressNotificationParam {
    ProgressNotificationParam::new(token, seq as f64).with_message(event.message())
}

/// Pick the sink for one tool call: a live [`ProgressReporter`] when the client
/// asked for progress (sent a token), else [`NullSink`]. Gating at construction
/// means the no-progress path never even allocates a counter or touches the peer.
fn progress_sink(peer: Peer<RoleServer>, meta: &RequestMetaObject) -> Arc<dyn ProgressSink> {
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
    use rmcp::model::{ContentBlock, NumberOrString};
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

    /// The deliberate pipeline's image hand-off, pinned across the server seam: a file
    /// the dossier sweep routes via the REAL `attach` tool must survive
    /// drain → stitch → lane dispatch → the offline synth's single turn. The engine
    /// tests pin each stage in isolation; this pins the hand-offs between them — the
    /// exact place a refactor could drop the `images` value between drain and dispatch
    /// with nothing failing (flagged by the DeepSeek cross-family review, 2026-07-26).
    #[tokio::test]
    async fn a_sweep_routed_image_survives_drain_stitch_and_direct_dispatch() {
        use crate::sweep_attach::{SweepAttach, SweepAttachArgs};
        use rig_agent::tool::{Tool as _, ToolContext};

        // A workspace holding one real-by-magic-bytes PNG and one text file.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend(std::iter::repeat_n(0xAB, 32));
        std::fs::write(root.join("arch.png"), &png).unwrap();
        std::fs::write(root.join("notes.md"), "the design in one line\n").unwrap();

        // The sink and consumer exactly as the `deliberate` handler builds them:
        // offline synth, vision on (the synth's cap is what admits the image).
        let consumer = SweepConsumer {
            kind: SweepConsumerKind::OfflineSynth,
            label: std::sync::Arc::from("the offline synth (`scripted-synth`)"),
            vision: true,
        };
        let sink = std::sync::Arc::new(SweepAttachSink::new(
            8,
            consumer.clone(),
            std::collections::HashSet::new(),
        ));

        // Route both files through the real tool — the same resolve/read/classify/
        // commit path a live explorer's `attach` call runs.
        let tool = SweepAttach::new(
            crate::sandbox::KaishWorker::spawn(&root).expect("spawn read-only worker"),
            &root,
            sink.clone(),
            std::sync::Arc::new(crate::progress::NullSink),
        );
        let receipt = tool
            .call(
                &mut ToolContext::new(),
                SweepAttachArgs {
                    paths: vec!["arch.png".into(), "notes.md".into()],
                    note: None,
                },
            )
            .await
            .expect("attach succeeds on both files");
        assert!(
            receipt.contains("attached: arch.png"),
            "the image must route: {receipt}"
        );

        // Drain + stitch + keep — the extracted handler step, in the handler's order.
        // Keeping BEFORE stitching would store a dossier missing the routed evidence the
        // synth then reasons over: an audit record that disagrees with what was sent.
        let store: Arc<crate::cas::MediaStore> = Arc::new(crate::cas::MediaStore::Memory(
            crate::cas::MemoryCas::new(None),
        ));
        let mut dossier = String::from("src/x.rs:1 DOSSIER");
        let (images, kept) = stitch_and_keep(
            &mut dossier,
            &consumer,
            Some(&sink),
            Some(&store),
            "does the diagram confirm it?",
            "scripted",
            "scripted-explorer",
        );
        let kept = kept.expect("a live store keeps the dossier");
        let stored = String::from_utf8(
            store
                .get(&crate::cas::Digest::from_hex(&kept.digest).unwrap())
                .expect("readable")
                .expect("present")
                .0,
        )
        .unwrap();
        assert_eq!(
            stored, dossier,
            "what is kept must be the STITCHED dossier — the exact text stage 2 receives"
        );
        assert!(
            stored.contains("notes.md"),
            "a dossier kept before stitching would be missing the routed evidence: {stored}"
        );
        assert!(
            dossier.contains("notes.md"),
            "the text body is stitched into the dossier: {dossier}"
        );
        assert!(
            dossier.contains("arch.png"),
            "the image is named in the dossier manifest: {dossier}"
        );
        assert_eq!(
            images.len(),
            1,
            "exactly the routed image reaches the lane dispatch"
        );

        // Lane dispatch, direct: the image must land on the synth's single turn as a
        // native image part.
        let client = crate::test_support::ScriptedClient::builder()
            .on_model("scripted-synth", |req| {
                let carries_image = req.chat_history.iter().any(|m| {
                    matches!(m, rig_core::completion::Message::User { content }
                    if content.iter().any(|c| matches!(
                        c,
                        rig_core::completion::message::UserContent::Image(_)
                    )))
                });
                assert!(
                    carries_image,
                    "the routed image must ride the synth's turn as an image part"
                );
                Ok(crate::test_support::text_response("DELIBERATION: seen"))
            })
            .build();
        let synth = crate::consult::Arm::new(
            client.clone(),
            "scripted-synth",
            1 << 14,
            None,
            crate::consult::ModelCaps {
                vision: true,
                tool_result_images: true,
            },
        );
        let (out, _usage) = crate::consult::deliberate_direct(
            "does the diagram confirm it?",
            &dossier,
            &images,
            &synth,
            "system",
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("scripted direct deliberation succeeds");
        assert!(out.contains("DELIBERATION"), "the answer came back: {out}");
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
        let prompt = crate::consult::consult_user_prompt("Assess it.", None, &[], &attachments);

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

    /// `handler_from_toml` with an EMPTY credential environment, so the usable-cast
    /// roster is exactly what the fixture declares. The built-in cast registry merges
    /// under every config, so without this a fixture's roster silently includes every
    /// built-in cast whose key happens to be set in the developer's shell — which is how
    /// a staffing test can pass on CI and fail on a maintainer's laptop. Fixtures that
    /// want a usable cast declare a `key_optional = true` backend (no credential needed);
    /// everything else resolves to `Unconfigured` and drops out.
    fn hermetic_handler_from_toml(toml: &str) -> KaiboHandler {
        KaiboHandler::new_with_env(Config::from_toml_str(toml).expect("config parses"), |_| {
            None
        })
        .expect("handler builds")
    }

    /// **The two-key gate on `save_artifact`, all four combinations.**
    ///
    /// A sink exists only when the operator enabled `[artifacts]`, the call passed
    /// `save_artifacts`, and the media CAS is live. No sink means the tool is not in the
    /// driver's toolset at all (`ConsultConfig::artifacts` is what `consult_tools` reads),
    /// so this is the whole gate — there is no second place it could leak through.
    ///
    /// A caller that asked and cannot be served gets a refusal naming which key is
    /// missing, never a quiet consult that swallowed the bulk it was told to store.
    #[test]
    fn save_artifact_needs_the_operator_key_the_call_key_and_a_live_cas() {
        let ask = |save_artifacts: bool| ConsultInput {
            question: "q".into(),
            context: None,
            path: None,
            cast: None,
            explorer_model: None,
            explorer_backend: None,
            synth_model: None,
            synth_backend: None,
            session_id: Some("sess-1".into()),
            explorer_max_turns: None,
            synth_max_turns: None,
            include_report: false,
            attach: Vec::new(),
            save_artifacts,
        };
        let sink = |h: &KaiboHandler, input: &ConsultInput| {
            h.artifact_sink(
                input.save_artifacts,
                &input.question,
                input.session_id.as_deref(),
                "deepseek",
                "deepseek/deepseek-v4-pro",
            )
        };

        // Operator key OFF (the default posture of every kaibo install).
        let off = handler();
        assert!(
            sink(&off, &ask(false))
                .expect("not asking is never an error")
                .is_none(),
            "no key asked, no sink"
        );
        let err = sink(&off, &ask(true)).expect_err("asking for a disabled capability must refuse");
        assert!(
            err.message.contains("[artifacts] enabled")
                && err.message.contains("--allow-save-artifact"),
            "the refusal names how an operator turns it on: {}",
            err.message
        );

        // Operator key ON, call key OFF: still no sink, and no error — the caller asked
        // for nothing.
        let on = handler_from_toml("[artifacts]\nenabled = true\n");
        assert!(
            sink(&on, &ask(false))
                .expect("not asking is never an error")
                .is_none(),
            "the operator's permission is standing, not automatic"
        );

        // Both keys, CAS live: a sink.
        assert!(
            sink(&on, &ask(true))
                .expect("both keys and a live CAS")
                .is_some(),
            "all three conditions hold, so the driver gets the tool"
        );

        // Both keys, CAS off: refused, naming the CAS rather than the artifacts flag.
        let no_cas = handler_from_toml("[artifacts]\nenabled = true\n\n[cas]\nenabled = false\n");
        let err = sink(&no_cas, &ask(true)).expect_err("no store means no artifact");
        assert!(
            err.message.contains("[cas] enabled = false"),
            "the refusal names the missing key precisely: {}",
            err.message
        );
    }

    /// A handler in real DISK mode at a temp dir, with the backing probe scripted — the
    /// one fact a test cannot arrange portably is what filesystem it is running on.
    fn disk_handler_with_probe(
        dir: &std::path::Path,
        probe: crate::cas::BackingProbe,
    ) -> KaiboHandler {
        let toml = format!("[cas]\ndir = \"{}\"\n", dir.join("cas").display());
        hermetic_handler_from_toml(&toml)
            .with_backing_probe(probe)
            .finalize_media_store(true)
            .expect("disk-backed store opens")
    }

    fn probe_overlayfs(_: &std::path::Path) -> crate::cas::Backing {
        crate::cas::Backing::Ephemeral { fs: "overlayfs" }
    }
    fn probe_durable(_: &std::path::Path) -> crate::cas::Backing {
        crate::cas::Backing::Durable
    }
    fn probe_unknown(_: &std::path::Path) -> crate::cas::Backing {
        crate::cas::Backing::Unknown
    }

    /// The `[cas]` section of a handler's rendered `kaibo://config`.
    fn cas_section(h: &KaiboHandler) -> String {
        let body = render_config_resource(
            &h.config,
            &[],
            None,
            false,
            vec![],
            true,
            h.live_cas_mode(),
            h.cas_ephemeral_fs(),
        );
        let start = body.find("[cas]").expect("a [cas] section");
        let rest = &body[start..];
        let end = rest[1..].find("\n[").map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// **Disk mode on an ephemeral filesystem is recorded, not just logged.**
    ///
    /// The scenario: a container with no volume mounted. Persistence comes up, the CAS
    /// opens, `mode = "disk"` — every signal says durable, and the whole store vanishes
    /// on exit. The startup warning is the loud channel, but startup log is exactly what
    /// an operator scrolls past or never sees when a client launched kaibo for them, so
    /// the finding also has to be somewhere they can query afterwards. That is the same
    /// reason memory mode reports `mode = "memory"` instead of only warning.
    #[test]
    fn an_ephemeral_cas_backing_is_recorded_and_surfaced_in_the_config_resource() {
        let dir = tempfile::tempdir().unwrap();
        let h = disk_handler_with_probe(dir.path(), probe_overlayfs);

        assert_eq!(h.live_cas_mode(), crate::config::CasMode::Disk);
        assert_eq!(
            h.cas_ephemeral_fs(),
            Some("overlayfs"),
            "the finding is kept, not discarded after the log line"
        );

        let cas = cas_section(&h);
        assert!(cas.contains("overlayfs"), "names the filesystem: {cas}");
        assert!(
            cas.to_uppercase().contains("EPHEMERAL"),
            "and says what that means: {cas}"
        );
        assert!(cas.to_lowercase().contains("volume"), "and the fix: {cas}");
        assert!(
            cas.contains("mode = \"disk\""),
            "beside the mode it is qualifying: {cas}"
        );
    }

    /// **A durable filesystem leaves no trace at all**, and neither does a probe that
    /// could not answer. An operator on ext4 must not gain a line, and an unreadable
    /// `f_type` establishes nothing — a guard that spoke on every failure to look would
    /// be ignored by the time it mattered.
    #[test]
    fn a_durable_or_unreadable_backing_leaves_the_config_untouched() {
        for (what, probe) in [
            ("durable", probe_durable as crate::cas::BackingProbe),
            ("unknown", probe_unknown as crate::cas::BackingProbe),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let h = disk_handler_with_probe(dir.path(), probe);
            assert_eq!(h.cas_ephemeral_fs(), None, "{what}: nothing to record");
            let cas = cas_section(&h);
            assert!(
                !cas.contains("backing"),
                "{what}: no backing line at all, got: {cas}"
            );
        }
    }

    /// The guard is disk-only. Memory mode already warns severely about exactly this
    /// loss, on its own terms; probing what a store that is not on a filesystem is
    /// sitting on would be a second warning about a fact the first one already owns.
    #[test]
    fn memory_mode_never_reports_a_backing_filesystem() {
        // Persistence inactive → memory mode, even though a dir resolves.
        let dir = tempfile::tempdir().unwrap();
        let toml = format!("[cas]\ndir = \"{}\"\n", dir.path().join("cas").display());
        let h = hermetic_handler_from_toml(&toml)
            .with_backing_probe(probe_overlayfs)
            .finalize_media_store(false)
            .expect("memory-mode handler");
        assert_eq!(h.live_cas_mode(), crate::config::CasMode::Memory);
        assert_eq!(
            h.cas_ephemeral_fs(),
            None,
            "memory mode owns its own durability warning; this guard stays out of it"
        );
    }

    /// The media-CAS lifecycle on the handler: `new` seeds the in-memory store when
    /// the CAS is enabled (mirroring sessions-start-in-memory) and holds nothing when
    /// `[cas] enabled = false`; `finalize_media_store` upgrades to disk exactly when
    /// persistence is active, keeps memory when it is not, and refuses a CAS dir
    /// inside an allowed tree the same way the session store does.
    #[test]
    fn media_store_lifecycle_memory_disk_and_off() {
        use crate::config::CasMode;

        // Enabled (default): a working in-memory store from construction.
        let h = handler();
        assert_eq!(h.live_cas_mode(), CasMode::Memory);
        assert!(h.media_store().is_some());

        // Explicitly disabled: no store at all.
        let off = handler_from_toml("[cas]\nenabled = false\n");
        assert_eq!(off.live_cas_mode(), CasMode::Off);
        assert!(off.media_store().is_none());
        // Finalize keeps it off even with persistence active — the off switch wins.
        let off = off.finalize_media_store(true).expect("finalize");
        assert_eq!(off.live_cas_mode(), CasMode::Off);

        // Persistence inactive: finalize keeps the in-memory store (the warned mode).
        let h = handler().finalize_media_store(false).expect("finalize");
        assert_eq!(h.live_cas_mode(), CasMode::Memory);

        // Persistence active + a dir outside every allowed tree: disk, rooted there.
        let cas_dir = tempfile::tempdir().unwrap();
        let toml = format!("[cas]\ndir = \"{}\"\n", cas_dir.path().display());
        let h = handler_from_toml(&toml)
            .finalize_media_store(true)
            .expect("finalize opens the disk store");
        assert_eq!(h.live_cas_mode(), CasMode::Disk);
        assert_eq!(
            h.media_store().unwrap().root().unwrap(),
            cas_dir.path(),
            "the disk store roots at the configured dir"
        );
    }

    /// A CAS dir that resolves inside an allowed project tree is refused at finalize —
    /// loudly, with the escape hatches named — never opened and never silently moved.
    #[test]
    fn media_store_refuses_a_cas_dir_inside_an_allowed_tree() {
        let root = tempfile::tempdir().unwrap();
        let toml = format!(
            "[server]\nroot = \"{r}\"\n[cas]\ndir = \"{r}/cas\"\n",
            r = root.path().display()
        );
        let Err(err) = handler_from_toml(&toml).finalize_media_store(true) else {
            panic!("a CAS inside the project must be refused");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("media CAS") && msg.contains("enabled = false"),
            "the error names the store and an escape hatch, got: {msg}"
        );
    }

    // --- The generate lane, driven offline through the media-arm seam -----------

    /// A cast that can staff `generate`: a keyless (placeholder-credential) stability
    /// backend plus an image-only cast. The scripted factory below never dials it.
    const MEDIA_CAST_TOML: &str = r#"
        [backends.sd]
        kind = "stability"
        key_optional = true

        [casts.artist]
        image = "sd/core"
    "#;

    /// A factory handing every build the same scripted [`crate::media::MediaModel`] —
    /// the offline double for the whole generate lane (CAS writes, job lane, render).
    struct ScriptedMediaArms(Arc<dyn crate::media::MediaModel>);

    impl crate::media::MediaArmFactory for ScriptedMediaArms {
        fn build(
            &self,
            _backend: &Backend,
            slot: &ModelSlot,
        ) -> anyhow::Result<crate::media::MediaArm> {
            Ok(crate::media::MediaArm::new(
                self.0.clone(),
                slot.qualified(),
            ))
        }
    }

    /// Completes synchronously with a fixed artifact list.
    struct SyncArtifacts(Vec<crate::media::MediaArtifact>);

    #[async_trait::async_trait]
    impl crate::media::MediaModel for SyncArtifacts {
        /// Stands in for Stability, the backend `inputs` exists for — so the handler
        /// tests exercise the accepting path rather than the arm's refusal.
        fn accepts_inputs(&self) -> bool {
            true
        }

        async fn generate(
            &self,
            _request: &crate::media::MediaRequest,
        ) -> anyhow::Result<crate::media::MediaOutcome> {
            Ok(crate::media::MediaOutcome::Complete(self.0.clone()))
        }

        async fn poll(
            &self,
            _job: &crate::media::MediaJobId,
        ) -> anyhow::Result<crate::media::MediaPollOutcome> {
            unreachable!("a sync generation is never polled")
        }
    }

    /// Defers on generate; the poll is pending once, then completes.
    struct DeferredArtifacts {
        artifacts: Vec<crate::media::MediaArtifact>,
        polls: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl crate::media::MediaModel for DeferredArtifacts {
        async fn generate(
            &self,
            _request: &crate::media::MediaRequest,
        ) -> anyhow::Result<crate::media::MediaOutcome> {
            Ok(crate::media::MediaOutcome::Deferred(
                crate::media::MediaJobId("prov-77".to_string()),
            ))
        }

        async fn poll(
            &self,
            job: &crate::media::MediaJobId,
        ) -> anyhow::Result<crate::media::MediaPollOutcome> {
            assert_eq!(job.0, "prov-77", "the provider id round-trips to the poll");
            let mut n = self.polls.lock().unwrap();
            *n += 1;
            Ok(if *n == 1 {
                crate::media::MediaPollOutcome::Pending
            } else {
                crate::media::MediaPollOutcome::Complete(self.artifacts.clone())
            })
        }
    }

    fn png(bytes: &[u8]) -> crate::media::MediaArtifact {
        crate::media::MediaArtifact {
            bytes: bytes.to_vec(),
            mime: "image/png".to_string(),
            seed: Some("42".to_string()),
        }
    }

    fn media_handler(model: Arc<dyn crate::media::MediaModel>) -> KaiboHandler {
        hermetic_handler_from_toml(MEDIA_CAST_TOML)
            .with_media_arms(Arc::new(ScriptedMediaArms(model)))
    }

    /// The sync lane: a completed generation stores EVERY artifact in the media store
    /// (its own digest and provenance each — the one-to-many contract) and answers with
    /// per-artifact `kaibo://cas/<digest>` URIs, never inline bytes. Memory mode, so no
    /// `path:` lines.
    #[tokio::test]
    async fn generate_stores_every_artifact_and_returns_digests() {
        let a1 = png(b"first-artifact");
        let mut a2 = png(b"second-artifact");
        a2.mime = "image/webp".to_string();
        a2.seed = None;
        let h = media_handler(Arc::new(SyncArtifacts(vec![a1.clone(), a2.clone()])));
        let result = h
            .generate(Parameters(GenerateInput {
                prompt: "a lighthouse at dusk".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("generate succeeds");
        assert_ne!(result.is_error, Some(true), "not a tool error");
        let text = result_text(result);

        let store = h.media_store().expect("cas on");
        for (artifact, ext) in [
            (&a1, crate::cas::Extension::Png),
            (&a2, crate::cas::Extension::Webp),
        ] {
            let digest = crate::cas::Digest::of_bytes(&artifact.bytes);
            assert!(
                text.contains(&format!("kaibo://cas/{}", digest.to_hex())),
                "the answer names each artifact's digest URI:\n{text}"
            );
            assert_eq!(
                store.get(&digest).expect("readable"),
                Some((artifact.bytes.clone(), ext)),
                "the bytes are in the store under their own digest"
            );
        }
        assert!(
            text.contains("seed 42"),
            "the provider seed is echoed:\n{text}"
        );
        assert!(
            !text.contains("path:"),
            "memory mode has no filesystem path to name:\n{text}"
        );
        assert!(
            text.contains("cast `artist`") && text.contains("sd/core"),
            "the provenance footer names the cast and image model:\n{text}"
        );
    }

    /// Provenance sidecars are per-artifact: the prompt, the image slot ref, the cast,
    /// the mime, and the provider seed all land beside the object.
    #[tokio::test]
    async fn generate_records_provenance_beside_each_artifact() {
        let artifact = png(b"provenanced");
        let h = media_handler(Arc::new(SyncArtifacts(vec![artifact.clone()])));
        h.generate(Parameters(GenerateInput {
            prompt: "a red door".to_string(),
            cast: Some("artist".to_string()),
            fields: None,
            inputs: None,
            op: None,
        }))
        .await
        .expect("generate succeeds");

        let digest = crate::cas::Digest::of_bytes(&artifact.bytes);
        let Some(crate::cas::MediaStore::Memory(mem)) = h.media_store().map(Arc::as_ref) else {
            panic!("test handler holds the in-memory store");
        };
        let prov = mem.provenance(&digest).expect("sidecar recorded");
        assert_eq!(prov.prompt, "a red door");
        assert_eq!(prov.model, "sd/core");
        assert_eq!(prov.cast, "artist");
        assert_eq!(prov.mime, "image/png");
        assert_eq!(prov.seed.as_deref(), Some("42"));
    }

    /// The deferred lane: the declared-deferred outcome becomes a `job-N` on the
    /// existing collect verbs; the background task owns the poll cadence
    /// (pending → sleep → complete) and the finished job's answer carries the digest
    /// URIs, with the artifacts landed in the store. `start_paused` auto-advances the
    /// poll interval's sleep.
    #[tokio::test(start_paused = true)]
    async fn generate_deferred_returns_a_job_that_collects_digests() {
        let artifact = png(b"deferred-artifact");
        let mut second = png(b"second-deferred-artifact");
        second.mime = "image/webp".to_string();
        let h = media_handler(Arc::new(DeferredArtifacts {
            artifacts: vec![artifact.clone(), second.clone()],
            polls: std::sync::Mutex::new(0),
        }));
        let result = h
            .generate(Parameters(GenerateInput {
                prompt: "slow art".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("submit succeeds");
        let ack = result_text(result);
        assert!(
            ack.contains("job-") && ack.contains("job_get"),
            "the ack hands back a job handle and the collect verb:\n{ack}"
        );
        let handle = ack
            .split('`')
            .nth(1)
            .expect("the handle is backticked")
            .to_string();

        let digest = crate::cas::Digest::of_bytes(&artifact.bytes);
        let mut answer = String::new();
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let r = h
                .job_get(Parameters(HandleInput {
                    handle: handle.clone(),
                }))
                .await
                .expect("job_get answers");
            let text = result_text(r);
            if text.contains("kaibo://cas/") {
                answer = text;
                break;
            }
        }
        assert!(
            answer.contains(&format!("kaibo://cas/{}", digest.to_hex())),
            "the finished job names the artifact's digest URI:\n{answer}"
        );
        assert_eq!(
            h.media_store().unwrap().get(&digest).expect("readable"),
            Some((artifact.bytes.clone(), crate::cas::Extension::Png)),
            "the deferred artifact landed in the store"
        );
        // The one-to-many contract holds through the deferred lane too: the second
        // artifact gets its own digest line and its own stored object.
        let second_digest = crate::cas::Digest::of_bytes(&second.bytes);
        assert!(
            answer.contains(&format!("kaibo://cas/{}", second_digest.to_hex())),
            "every artifact of a deferred completion is named:\n{answer}"
        );
        assert_eq!(
            h.media_store()
                .unwrap()
                .get(&second_digest)
                .expect("readable"),
            Some((second.bytes.clone(), crate::cas::Extension::Webp)),
            "the second deferred artifact landed in the store"
        );
    }

    /// Recorded provenance must describe the request that ran (or-gpt review,
    /// 2026-08-03): a caller who smuggles `prompt` or `model` through `fields` would
    /// make the sidecar record the *other* prompt/model — so both keys are reserved
    /// and refused loudly, naming the right parameter.
    #[tokio::test]
    async fn generate_refuses_reserved_field_keys() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![png(b"x")])));

        let err = h
            .generate(Parameters(GenerateInput {
                prompt: "A".to_string(),
                cast: Some("artist".to_string()),
                fields: Some(
                    [(
                        "prompt".to_string(),
                        GenerateFieldValue::Str("B".to_string()),
                    )]
                    .into_iter()
                    .collect(),
                ),
                inputs: None,
                op: None,
            }))
            .await
            .expect_err("a fields.prompt override must be refused");
        assert!(
            err.message.contains("prompt") && err.message.contains("reserved"),
            "the refusal names the reserved key, got: {}",
            err.message
        );

        let err = h
            .generate(Parameters(GenerateInput {
                prompt: "A".to_string(),
                cast: Some("artist".to_string()),
                fields: Some(
                    [(
                        "model".to_string(),
                        GenerateFieldValue::Str("sd3.5-large".to_string()),
                    )]
                    .into_iter()
                    .collect(),
                ),
                inputs: None,
                op: None,
            }))
            .await
            .expect_err("a fields.model override must be refused");
        assert!(
            err.message.contains("model")
                && err.message.contains("reserved")
                && err.message.contains("image"),
            "the refusal names the key and points at the image slot, got: {}",
            err.message
        );
    }

    /// A provider reporting success with an empty artifact list is that provider's
    /// bug, surfaced as a loud tool error — never an empty success.
    #[tokio::test]
    async fn generate_refuses_an_empty_artifact_list() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let result = h
            .generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("a tool-result error, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        assert!(
            result_text(result).contains("zero artifacts"),
            "the error names the empty completion"
        );
    }

    /// Partial multi-artifact failure must not orphan paid artifacts silently
    /// (or-gpt review, 2026-08-03). The cheap 90%: the WHOLE list is prevalidated —
    /// every mime must map onto the store's on-disk set — before the first byte is
    /// written, so [valid, unsupported] stores NOTHING instead of storing #1 and then
    /// discarding its digest with the error.
    #[tokio::test]
    async fn generate_prevalidates_every_mime_before_storing_anything() {
        let good = png(b"good-artifact");
        let mut bad = png(b"unstorable-artifact");
        bad.mime = "audio/mpeg".to_string();
        let h = media_handler(Arc::new(SyncArtifacts(vec![good.clone(), bad])));
        let result = h
            .generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("a tool-result error, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        assert!(
            result_text(result).contains("audio/mpeg"),
            "the error names the unstorable mime"
        );
        let store = h.media_store().unwrap();
        assert_eq!(
            store
                .get(&crate::cas::Digest::of_bytes(&good.bytes))
                .expect("readable"),
            None,
            "prevalidation failed the call BEFORE the valid artifact was stored"
        );
    }

    /// The residual 10%: a mid-loop store failure AFTER prevalidation (here the soft
    /// cap refusing artifact #2) must return an error that NAMES the digests already
    /// stored — the caller paid for them and they are retrievable; discarding their
    /// addresses would orphan them.
    #[tokio::test]
    async fn generate_mid_loop_store_failure_names_the_digests_already_stored() {
        let small = png(b"tiny");
        let big = png(&[7u8; 4096]);
        // A capped in-memory store: #1 fits, #2 breaches. Both mimes prevalidate.
        // The budget has to clear #1's content PLUS its provenance — memory-mode
        // admission counts the serialized provenance it stores, the same footprint disk
        // mode has always counted (a cap that ignored it meant two different things in
        // the two modes). 2 KiB leaves room for one small artifact and its record, and
        // none for a 4 KiB second.
        let toml = format!("{MEDIA_CAST_TOML}\n[cas]\nmax_bytes = 2048\n");
        let h = hermetic_handler_from_toml(&toml).with_media_arms(Arc::new(ScriptedMediaArms(
            Arc::new(SyncArtifacts(vec![small.clone(), big])),
        )));
        let result = h
            .generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("a tool-result error, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        let text = result_text(result);
        let stored = crate::cas::Digest::of_bytes(&small.bytes);
        assert!(
            text.contains(&stored.to_hex()),
            "the error names the digest that DID land, so it isn't orphaned:\n{text}"
        );
        assert!(
            h.media_store()
                .unwrap()
                .get(&stored)
                .expect("readable")
                .is_some(),
            "and that artifact really is retrievable"
        );
    }

    /// The deferred poll loop is bounded by [defaults] call_deadline: a provider job
    /// that never completes fails the kaibo job with the PROVIDER id named, so the
    /// operator can chase it on the provider's side.
    #[tokio::test(start_paused = true)]
    async fn generate_deferred_poll_deadline_fails_the_job_naming_the_provider_id() {
        /// Defers, then reports Pending forever.
        struct NeverDone;

        #[async_trait::async_trait]
        impl crate::media::MediaModel for NeverDone {
            async fn generate(
                &self,
                _request: &crate::media::MediaRequest,
            ) -> anyhow::Result<crate::media::MediaOutcome> {
                Ok(crate::media::MediaOutcome::Deferred(
                    crate::media::MediaJobId("prov-stuck-9".to_string()),
                ))
            }

            async fn poll(
                &self,
                _job: &crate::media::MediaJobId,
            ) -> anyhow::Result<crate::media::MediaPollOutcome> {
                Ok(crate::media::MediaPollOutcome::Pending)
            }
        }

        let h = media_handler(Arc::new(NeverDone));
        let ack = result_text(
            h.generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("submit succeeds"),
        );
        let handle = ack
            .split('`')
            .nth(1)
            .expect("backticked handle")
            .to_string();

        // Advance past the whole call_deadline budget (auto-advanced under start_paused)
        // and collect the failure.
        let deadline = h.config.defaults.call_deadline;
        let mut failed = String::new();
        for _ in 0..80 {
            tokio::time::sleep(deadline / 8).await;
            let r = h
                .job_get(Parameters(HandleInput {
                    handle: handle.clone(),
                }))
                .await
                .expect("job_get answers");
            if r.is_error == Some(true) {
                failed = result_text(r);
                break;
            }
        }
        assert!(
            failed.contains("prov-stuck-9") && failed.contains("still pending"),
            "the timed-out job names the provider id so it can be chased:\n{failed}"
        );
    }

    /// A cast without an `image` slot is refused at call time with the requirement
    /// named — the call-time mirror of the staffing rule.
    #[tokio::test]
    async fn generate_refuses_a_cast_without_an_image_slot() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![png(b"x")])));
        let err = h
            .generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("anthropic".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect_err("no image slot must refuse");
        assert!(
            err.message.contains("image") && err.message.contains("anthropic"),
            "the refusal names the missing slot and the cast, got: {}",
            err.message
        );
    }

    /// The advertisement gate, all three legs: no image-slot cast → dropped; staffable
    /// → advertised (and the job verbs come alive with it, since a deferred generate
    /// mints a `job-N`); staffable but `[cas] enabled = false` → dropped again, because
    /// artifacts would have nowhere to land.
    #[test]
    fn generate_is_advertised_only_with_an_image_cast_and_the_cas_on() {
        let bare = hermetic_handler_from_toml("");
        assert!(
            !bare.advertised_tools().contains(&"generate".to_string()),
            "no built-in cast has an image slot, so a stock install must not advertise it"
        );

        let staffed = hermetic_handler_from_toml(MEDIA_CAST_TOML);
        let tools = staffed.advertised_tools();
        assert!(tools.contains(&"generate".to_string()));
        assert!(
            tools.contains(&"job_get".to_string()) && tools.contains(&"job_wait".to_string()),
            "generate is a job producer, so the collect verbs follow it: {tools:?}"
        );

        let cas_off = hermetic_handler_from_toml(&format!(
            "{MEDIA_CAST_TOML}
[cas]
enabled = false
"
        ));
        assert!(
            !cas_off.advertised_tools().contains(&"generate".to_string()),
            "with the CAS off the tool has nowhere to store artifacts and must vanish"
        );
    }

    /// A generate-ONLY server (consult, deliberate, and batch all disabled) still
    /// lists, collects, and cancels the `job-N` handles its deferred generations mint.
    /// The regression this pins (or-gpt review, 2026-08-03): `job_list`'s in-memory
    /// section was keyed off `consult || deliberate` flags, so a generate-only server
    /// advertised `job_list` yet returned no jobs section at all. And the shared job-N
    /// wording said "Consultation" for every producer — it must stay producer-neutral.
    #[tokio::test(start_paused = true)]
    async fn generate_only_server_lists_collects_and_cancels_its_jobs() {
        let toml = format!(
            "{MEDIA_CAST_TOML}\n[server.tools]\nconsult = false\ndeliberate = false\nbatch = false\n"
        );
        let h = hermetic_handler_from_toml(&toml).with_media_arms(Arc::new(ScriptedMediaArms(
            Arc::new(DeferredArtifacts {
                artifacts: vec![png(b"listed-artifact")],
                polls: std::sync::Mutex::new(0),
            }),
        )));
        assert!(
            h.advertised_tools().contains(&"job_list".to_string()),
            "generate keeps the collect verbs alive"
        );
        let ack = result_text(
            h.generate(Parameters(GenerateInput {
                prompt: "p".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: None,
                op: None,
            }))
            .await
            .expect("submit succeeds"),
        );
        let handle = ack
            .split('`')
            .nth(1)
            .expect("backticked handle")
            .to_string();

        // job_list must show the in-memory jobs section with this job in it.
        let listing = result_text(
            h.job_list(Parameters(ListInput {
                backend: None,
                all: false,
            }))
            .await
            .expect("job_list answers"),
        );
        assert!(
            listing.contains(&handle),
            "a generate-only server must list its own deferred job `{handle}`:\n{listing}"
        );

        // job_get while running: producer-neutral wording, never "Consultation".
        let running = result_text(
            h.job_get(Parameters(HandleInput {
                handle: handle.clone(),
            }))
            .await
            .expect("job_get answers"),
        );
        assert!(
            !running.contains("Consultation"),
            "job-N wording is shared by every producer and must stay neutral:\n{running}"
        );

        // job_cancel: same neutrality.
        let canceled = result_text(
            h.job_cancel(Parameters(HandleInput {
                handle: handle.clone(),
            }))
            .await
            .expect("job_cancel answers"),
        );
        assert!(
            canceled.contains(&handle) && !canceled.contains("Consultation"),
            "the cancel ack names the job without calling it a consultation:\n{canceled}"
        );
    }

    /// A minimal textual-artifact sidecar for driving the resource read directly.
    fn textual_provenance() -> crate::cas::Provenance {
        crate::cas::Provenance {
            prompt: "q".to_string(),
            model: "m".to_string(),
            cast: "c".to_string(),
            timestamp: 1,
            mime: "text/plain; charset=utf-8".to_string(),
            seed: None,
            tool: Some("save_artifact".to_string()),
            slot: Some("synth".to_string()),
            label: Some("a test artifact".to_string()),
            session: None,
        }
    }

    /// Call `read_cas` and unwrap the success, for the many shapes below.
    async fn read(
        h: &KaiboHandler,
        digest: &str,
        offset: Option<usize>,
        length: Option<usize>,
    ) -> CallToolResult {
        h.read_cas(Parameters(ReadCasInput {
            digest: digest.to_string(),
            offset,
            length,
        }))
        .await
        .expect("a stored digest reads")
    }

    /// The metadata block — always the FIRST content block, on every response.
    fn meta_of(r: &CallToolResult) -> String {
        r.content
            .first()
            .and_then(|c| c.as_text().map(|t| t.text.clone()))
            .expect("every response leads with metadata")
    }

    /// A handler with the media CAS on and one textual artifact stored; returns the hex.
    fn with_text_artifact(h: &KaiboHandler, content: &[u8]) -> String {
        h.media_store()
            .expect("media CAS is on")
            .put(content, crate::cas::Extension::Txt, &textual_provenance())
            .expect("put succeeds")
            .to_hex()
    }

    /// **Metadata leads, and `length: 0` is the cheap HEAD.** The whole point of
    /// replacing `resources/read`: ask what an object is for a few dozen tokens instead
    /// of pulling it to find out. The label the sidecar carries rides along, so a caller
    /// can tell one digest from another without reading either.
    #[tokio::test]
    async fn read_cas_length_zero_returns_metadata_only() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let hex = with_text_artifact(&h, &b"z".repeat(9000));
        let r = read(&h, &hex, None, Some(0)).await;
        assert_eq!(r.content.len(), 1, "metadata only, no body block");
        let meta = meta_of(&r);
        for needle in [
            hex.as_str(),
            "mime: text/plain",
            "bytes: 9000",
            "binary: false",
            "label: a test artifact",
        ] {
            assert!(
                meta.contains(needle),
                "metadata must carry {needle}: {meta}"
            );
        }
    }

    /// **The default read is bounded.** An omitted `length` returns the first window and
    /// says how much more there is — the behavior `resources/read` structurally could not
    /// offer, and the reason a 3.8 MB artifact no longer lands whole in a context window.
    /// Paging with `offset` reassembles the object exactly.
    #[tokio::test]
    async fn read_cas_defaults_to_a_bounded_window_and_pages_with_offset() {
        use super::cas_read::DEFAULT_READ_BYTES;
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let total = DEFAULT_READ_BYTES + 250;
        let content: Vec<u8> = (0..total).map(|i| b'a' + (i % 26) as u8).collect();
        let hex = with_text_artifact(&h, &content);

        let first = read(&h, &hex, None, None).await;
        let head = first.content[1].as_text().expect("text body").text.clone();
        assert_eq!(
            head.len(),
            DEFAULT_READ_BYTES,
            "the default window, not the whole object"
        );
        assert!(
            meta_of(&first).contains(&total.to_string()),
            "and the total, so paging is informed: {}",
            meta_of(&first)
        );

        let second = read(&h, &hex, Some(DEFAULT_READ_BYTES), None).await;
        let tail = second.content[1].as_text().expect("text body").text.clone();
        assert_eq!(
            format!("{head}{tail}").as_bytes(),
            content.as_slice(),
            "the pages reassemble the artifact byte for byte"
        );
    }

    /// An offset past the end is a clean empty response, not a failure — a paging loop
    /// finds the end by reading past it.
    #[tokio::test]
    async fn read_cas_offset_past_the_end_is_empty_not_an_error() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let hex = with_text_artifact(&h, b"tiny");
        let r = read(&h, &hex, Some(4096), None).await;
        assert_eq!(r.content.len(), 1, "metadata only");
        assert!(meta_of(&r).contains("bytes: 4"), "{}", meta_of(&r));
    }

    /// A `length` past the ceiling is refused, naming the ceiling — never silently
    /// clamped, which would leave the caller's next offset wrong.
    #[tokio::test]
    async fn read_cas_refuses_a_length_past_the_ceiling() {
        use super::cas_read::MAX_READ_BYTES;
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let hex = with_text_artifact(&h, b"tiny");
        let err = h
            .read_cas(Parameters(ReadCasInput {
                digest: hex,
                offset: None,
                length: Some(MAX_READ_BYTES + 1),
            }))
            .await
            .expect_err("past the ceiling is refused");
        assert!(
            err.message.contains(&MAX_READ_BYTES.to_string()),
            "the refusal names the ceiling: {}",
            err.message
        );
    }

    /// A textual artifact arrives as **text**, not base64 to decode. The producers mark
    /// what they make (`save_artifact` writes text formats, `generate` writes images) and
    /// the sidecar mime carries that mark to retrieval.
    #[tokio::test]
    async fn read_cas_serves_textual_artifacts_as_text() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let hex = with_text_artifact(&h, b"line one\nline two\n");
        let r = read(&h, &hex, None, None).await;
        assert_eq!(
            r.content[1].as_text().expect("text body").text,
            "line one\nline two\n"
        );
    }

    /// Bytes stored under a textual extension that are not UTF-8 fall back to base64 of
    /// exactly those bytes — never a lossy decode, which would hand back content the
    /// store does not hold.
    #[tokio::test]
    async fn read_cas_falls_back_to_base64_for_undecodable_text() {
        use base64::Engine as _;
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let bytes: &[u8] = &[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72];
        let hex = with_text_artifact(&h, bytes);
        let r = read(&h, &hex, None, None).await;
        let body = r.content[1].as_text().expect("base64 body").text.clone();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&body)
                .expect("valid base64"),
            bytes
        );
    }

    /// **A small image takes one hop to the eye**: an image content block, which hosts
    /// render straight to a vision model. This is the retrieval shape `generate`'s output
    /// actually wants, and the one a resource read could never produce.
    #[tokio::test]
    async fn read_cas_returns_a_small_image_as_an_image_block() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![png(b"pretend-png-bytes")])));
        h.generate(Parameters(GenerateInput {
            prompt: "p".to_string(),
            cast: Some("artist".to_string()),
            fields: None,
            inputs: None,
            op: None,
        }))
        .await
        .expect("generate succeeds");
        let hex = crate::cas::Digest::of_bytes(b"pretend-png-bytes").to_hex();

        let r = read(&h, &hex, None, None).await;
        assert!(
            meta_of(&r).contains("binary: true"),
            "metadata still leads: {}",
            meta_of(&r)
        );
        let image = r.content[1].as_image().expect("an image content block");
        assert_eq!(image.mime_type, "image/png");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .expect("valid base64"),
            b"pretend-png-bytes"
        );
    }

    /// **An image past the inline threshold is metadata only** — no image block, and no
    /// base64 wall either. The measured failure this whole tool exists to prevent: a
    /// 3.8 MB PNG became ~5 MB of base64 through the resource route. Over the threshold
    /// the useful move is the path, not the bytes.
    #[tokio::test]
    async fn read_cas_refuses_to_inline_an_image_past_the_threshold() {
        use super::cas_read::INLINE_IMAGE_MAX_BYTES;
        let big = vec![3u8; INLINE_IMAGE_MAX_BYTES + 1];
        let h = media_handler(Arc::new(SyncArtifacts(vec![png(&big)])));
        h.generate(Parameters(GenerateInput {
            prompt: "p".to_string(),
            cast: Some("artist".to_string()),
            fields: None,
            inputs: None,
            op: None,
        }))
        .await
        .expect("generate succeeds");
        let hex = crate::cas::Digest::of_bytes(&big).to_hex();

        let r = read(&h, &hex, None, None).await;
        assert_eq!(
            r.content.len(),
            1,
            "metadata only — neither an image block nor a base64 dump"
        );
        assert!(meta_of(&r).contains("binary: true"));

        // But an explicit range still works: the caller asked, so it gets bytes.
        let ranged = read(&h, &hex, Some(0), Some(8)).await;
        assert_eq!(ranged.content.len(), 2, "an explicit range is served");
    }

    /// Disk mode names the real file, so a caller holding a shell can go direct instead
    /// of paging megabytes through a model. Memory mode has no file and says nothing.
    #[tokio::test]
    async fn read_cas_metadata_carries_the_path_only_in_disk_mode() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "{MEDIA_CAST_TOML}\n[cas]\ndir = \"{}\"\n",
            dir.path().join("cas").display()
        );
        let h = hermetic_handler_from_toml(&toml)
            .finalize_media_store(true)
            .expect("disk-backed store");
        let hex = with_text_artifact(&h, b"on disk");
        let meta = meta_of(&read(&h, &hex, None, Some(0)).await);
        assert!(meta.contains("path: "), "disk mode names the file: {meta}");
        assert!(meta.contains(".txt"), "with its real extension: {meta}");

        let mem = media_handler(Arc::new(SyncArtifacts(vec![])));
        let mem_hex = with_text_artifact(&mem, b"in memory");
        let mem_meta = meta_of(&read(&mem, &mem_hex, None, Some(0)).await);
        assert!(
            !mem_meta.contains("path: "),
            "memory mode has no file to name: {mem_meta}"
        );
    }

    /// **An object whose sidecar is gone still reads, and says its record is gone.**
    ///
    /// This is a real state, not a hypothetical: `Cas::put` writes the object first and
    /// the sidecar second, so a failure between them leaves exactly this, and
    /// `Cas::entry_for`'s probe fallback is what keeps such an object reachable. The store
    /// never rewrites, so it stays recordless — a caller deciding whether to trust the
    /// bytes deserves to know that, rather than reading "no label" as "nothing to say".
    #[tokio::test]
    async fn read_cas_reports_an_object_whose_provenance_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "{MEDIA_CAST_TOML}\n[cas]\ndir = \"{}\"\n",
            dir.path().join("cas").display()
        );
        let h = hermetic_handler_from_toml(&toml)
            .finalize_media_store(true)
            .expect("disk-backed store");
        let hex = with_text_artifact(&h, b"the object outlives its record");
        let digest = crate::cas::Digest::from_hex(&hex).unwrap();

        // With the sidecar in place: the label rides, and nothing claims a missing record.
        let meta = meta_of(&read(&h, &hex, None, Some(0)).await);
        assert!(meta.contains("label: a test artifact"), "{meta}");
        assert!(!meta.contains("provenance:"), "{meta}");

        // Now take the record away, leaving the object.
        let object = h.media_store().unwrap().path_for(&digest).unwrap();
        std::fs::remove_file(object.with_extension("json")).expect("remove the sidecar");

        let r = read(&h, &hex, None, None).await;
        let meta = meta_of(&r);
        assert!(
            meta.contains("provenance: absent"),
            "the missing record is stated: {meta}"
        );
        assert!(!meta.contains("label:"), "and no label is invented: {meta}");
        assert_eq!(
            r.content[1].as_text().expect("text body").text,
            "the object outlives its record",
            "and the bytes still come back — a lost record is not a lost object"
        );
    }

    /// The digest is validated before it can touch a lookup, an unknown one is a clean
    /// not-found, and neither folds into the other. Ported from the resource this tool
    /// replaced — the guarantees survive the change of surface.
    #[tokio::test]
    async fn read_cas_validates_the_digest_and_reports_a_miss_cleanly() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));

        let missing = crate::cas::Digest::of_bytes(b"never-produced").to_hex();
        let err = h
            .read_cas(Parameters(ReadCasInput {
                digest: missing,
                offset: None,
                length: None,
            }))
            .await
            .expect_err("unknown digest is not found");
        assert!(err.message.contains("no artifact"), "got: {}", err.message);

        for bad in ["../../etc/passwd", "ABCD", "", &"f".repeat(63)] {
            let err = h
                .read_cas(Parameters(ReadCasInput {
                    digest: bad.to_string(),
                    offset: None,
                    length: None,
                }))
                .await
                .expect_err("a malformed digest never reaches a lookup");
            assert!(
                err.message.contains("64 lowercase hex"),
                "the refusal names the rule, got: {}",
                err.message
            );
        }
    }

    /// A corrupt object stays a LOUD, distinct failure — never folded into "not found",
    /// which would let real corruption read as an ordinary absence.
    #[tokio::test]
    async fn read_cas_surfaces_a_corrupt_object_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "{MEDIA_CAST_TOML}\n[cas]\ndir = \"{}\"\n",
            dir.path().join("cas").display()
        );
        let h = hermetic_handler_from_toml(&toml)
            .finalize_media_store(true)
            .expect("disk-backed store");
        let hex = with_text_artifact(&h, b"honest content");
        let digest = crate::cas::Digest::from_hex(&hex).unwrap();
        let path = h.media_store().unwrap().path_for(&digest).unwrap();
        std::fs::write(&path, b"tampered content").unwrap();

        let err = h
            .read_cas(Parameters(ReadCasInput {
                digest: hex,
                offset: None,
                length: None,
            }))
            .await
            .expect_err("a corrupt object is an error");
        assert!(
            err.message.contains("corrupt"),
            "and it says so rather than reporting a miss: {}",
            err.message
        );
    }

    /// Records the request the arm handed the provider, so the handler's wiring is
    /// observable rather than inferred from a success.
    #[derive(Default)]
    struct RecordingProvider(std::sync::Mutex<Option<crate::media::MediaRequest>>);

    #[async_trait::async_trait]
    impl crate::media::MediaModel for RecordingProvider {
        fn accepts_inputs(&self) -> bool {
            true
        }

        async fn generate(
            &self,
            request: &crate::media::MediaRequest,
        ) -> anyhow::Result<crate::media::MediaOutcome> {
            *self.0.lock().unwrap() = Some(request.clone());
            Ok(crate::media::MediaOutcome::Complete(vec![png(b"result")]))
        }

        async fn poll(
            &self,
            _job: &crate::media::MediaJobId,
        ) -> anyhow::Result<crate::media::MediaPollOutcome> {
            unreachable!("this test never defers")
        }
    }

    /// **The whole `inputs` path, driven through the tool face.**
    ///
    /// `resolve_inputs` is unit-tested on its own, but nothing proved the handler
    /// actually threads a caller's digest into the request the provider receives — the
    /// wiring is exactly what a later refactor drops silently, because dropping it still
    /// returns a plausible image. This asserts the provider saw the part, under the
    /// caller's field name, with the bytes and the format the store recorded.
    #[tokio::test]
    async fn generate_threads_caller_digests_into_the_providers_request() {
        let provider = Arc::new(RecordingProvider::default());
        let h = media_handler(provider.clone());
        // Seed the store the way `write_cas` would, then hand `generate` that digest.
        let digest = h
            .media_store()
            .expect("media CAS is on")
            .put(
                b"\x89PNG\r\n\x1a\nsource",
                crate::cas::Extension::Png,
                &textual_provenance(),
            )
            .expect("seeded")
            .to_hex();

        h.generate(Parameters(GenerateInput {
            prompt: "erase the sign".to_string(),
            cast: Some("artist".to_string()),
            fields: None,
            inputs: Some([("image".to_string(), digest)].into_iter().collect()),
            op: None,
        }))
        .await
        .expect("a stored digest is a usable input");

        let seen = provider.0.lock().unwrap().clone().expect("provider was called");
        assert_eq!(seen.inputs.len(), 1, "the part reached the provider");
        assert_eq!(seen.inputs[0].field, "image", "under the caller's field name");
        assert_eq!(seen.inputs[0].bytes, b"\x89PNG\r\n\x1a\nsource");
        assert_eq!(
            seen.inputs[0].filename, "image.png",
            "named from the store's record, not the caller's belief"
        );
        assert_eq!(seen.inputs[0].mime, "image/png");
    }

    /// A digest the store does not hold refuses at the tool face, before any provider
    /// request is made — so a typo never costs a generation.
    #[tokio::test]
    async fn generate_refuses_an_unknown_input_digest_without_calling_the_provider() {
        let provider = Arc::new(RecordingProvider::default());
        let h = media_handler(provider.clone());
        let absent = crate::cas::Digest::of_bytes(b"never stored").to_hex();

        let err = h
            .generate(Parameters(GenerateInput {
                prompt: "erase the sign".to_string(),
                cast: Some("artist".to_string()),
                fields: None,
                inputs: Some([("image".to_string(), absent)].into_iter().collect()),
                op: None,
            }))
            .await
            .expect_err("an absent digest is refused");
        assert!(err.message.contains("holds no object"), "{}", err.message);
        assert!(
            provider.0.lock().unwrap().is_none(),
            "the provider must never have been called"
        );
    }

    /// A minimal but real PNG header — enough that `sniff_image` reads a true signature.
    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        v
    }

    fn write_cas_input(path: Option<&str>, content: Option<&str>) -> WriteCasInput {
        WriteCasInput {
            path: path.map(str::to_string),
            content: content.map(str::to_string),
            label: None,
        }
    }

    /// **`path` is the working route: kaibo reads the file itself.**
    ///
    /// The reason this exists at all — tool-call arguments are completion tokens the
    /// caller emits, so a real screenshot through `content` is megabytes the client has
    /// to write out one token at a time. This proves the cheap route round-trips.
    #[tokio::test]
    async fn write_cas_stores_a_file_the_caller_names_by_path() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let img = dir.path().join("screenshot.png");
        std::fs::write(&img, png_bytes()).expect("fixture writes");
        let h = handler_from_toml(&format!(
            "[server]\nallow_paths = [{:?}]\n",
            dir.path().display().to_string()
        ));

        let result = h
            .write_cas(Parameters(write_cas_input(
                Some(&img.display().to_string()),
                None,
            )))
            .await
            .expect("a contained png stores");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text().map(|t| t.text.clone()))
            .expect("write_cas answers with text");
        assert!(text.contains("image/png"), "{text}");
        assert!(text.contains(CAS_RES_PREFIX), "{text}");
    }

    /// The boundary is not ceremony here: the client is itself a model, and a
    /// prompt-injected one asking kaibo to pull a private key into the store and read it
    /// back is exactly this shape. A path outside the allowed set is refused, and the
    /// refusal names the boundary.
    #[tokio::test]
    async fn write_cas_refuses_a_path_outside_the_allowed_set() {
        let allowed = tempfile::TempDir::new().expect("temp dir");
        let elsewhere = tempfile::TempDir::new().expect("temp dir");
        let img = elsewhere.path().join("secret.png");
        std::fs::write(&img, png_bytes()).expect("fixture writes");
        let h = handler_from_toml(&format!(
            "[server]\nallow_paths = [{:?}]\n",
            allowed.path().display().to_string()
        ));

        let err = h
            .write_cas(Parameters(write_cas_input(
                Some(&img.display().to_string()),
                None,
            )))
            .await
            .expect_err("an out-of-tree path is refused");
        assert!(
            err.message.contains("outside the allowed set"),
            "the refusal must name the boundary: {}",
            err.message
        );
    }

    /// Two sources is a caller who said it twice. kaibo will not pick one — a silent
    /// precedence rule is how the stored bytes come to disagree with the intent.
    #[tokio::test]
    async fn write_cas_refuses_both_path_and_content() {
        let h = handler();
        let err = h
            .write_cas(Parameters(write_cas_input(Some("a.png"), Some("AAAA"))))
            .await
            .expect_err("two sources is ambiguous");
        assert!(err.message.contains("not both"), "{}", err.message);
    }

    #[tokio::test]
    async fn write_cas_refuses_neither_path_nor_content() {
        let h = handler();
        let err = h
            .write_cas(Parameters(write_cas_input(None, None)))
            .await
            .expect_err("no source is nothing to store");
        assert!(
            err.message.contains("`path`") && err.message.contains("`content`"),
            "the refusal names both ways in: {}",
            err.message
        );
    }

    /// **A server whose whole surface is the media store's two halves does nothing, and
    /// says so.**
    ///
    /// `has_substantive_tools` is what `main`'s empty-surface guard actually asks, and
    /// until this test it was only exercised through configurations the *flag* guard
    /// rejects first — so the follower rule itself was never pinned. Build the real
    /// degenerate server: every flag off, no staffable cast, media CAS on. It advertises
    /// exactly the store's deposit and retrieval verbs, both are followers, and the
    /// answer is no.
    ///
    /// This matters because `read_cas` and `write_cas` ride a store that is ON by
    /// default. Counted as substantive, either would keep the guard from ever firing on a
    /// stock install — a check that cannot fire protects nothing. `write_cas` is the
    /// sharper case: it *accepts* content rather than only handing back what earlier runs
    /// produced, so "it does something" is superficially true — but a server that can
    /// only hold your bytes and give them back has still investigated, answered, and
    /// generated nothing.
    #[test]
    fn a_surface_of_only_the_media_store_is_not_substantive() {
        let h = hermetic_handler_from_toml(
            "[server.tools]\nconsult = false\nexplore = false\ndeliberate = false\n\
             oneshot = false\nrun_kaish = false\nbatch = false\nlist_models = false\n\
             generate = false\n",
        );
        assert_eq!(
            h.advertised_tools(),
            vec!["read_cas".to_string(), "write_cas".to_string()],
            "the degenerate surface is exactly the two followers"
        );
        assert!(
            !h.has_substantive_tools(),
            "a server that can only hold bytes and hand them back is the useless server \
             the startup guard exists to refuse"
        );

        // And the same handler with one castless tool back on IS substantive — the rule
        // is about followers, not about being small.
        let with_shell = hermetic_handler_from_toml(
            "[server.tools]\nconsult = false\nexplore = false\ndeliberate = false\n\
             oneshot = false\nbatch = false\nlist_models = false\ngenerate = false\n",
        );
        assert!(
            with_shell.has_substantive_tools(),
            "run_kaish alone is a narrow server, not an empty one: {:?}",
            with_shell.advertised_tools()
        );
    }

    /// The media store's two halves are advertised exactly when it is live — the same
    /// liveness the resource `read_cas` replaced keyed on. Neither takes a cast, so
    /// nothing else gates either, and they move together: one `[cas] enabled` switch, not
    /// two ways to say the same thing.
    #[test]
    fn the_media_store_verbs_are_advertised_only_while_the_cas_is_on() {
        let on = handler();
        for verb in ["read_cas", "write_cas"] {
            assert!(
                on.advertised_tools().contains(&verb.to_string()),
                "a live CAS advertises {verb}, got {:?}",
                on.advertised_tools()
            );
        }
        let off = handler_from_toml("[cas]\nenabled = false\n");
        for verb in ["read_cas", "write_cas"] {
            assert!(
                !off.advertised_tools().contains(&verb.to_string()),
                "no store, no {verb}, got {:?}",
                off.advertised_tools()
            );
        }
    }

    /// **The `kaibo://cas/<digest>` RESOURCE is gone.** Retrieval is a tool now, and the
    /// route it replaced must not linger: a resource read of that URI is refused like any
    /// other unknown URI, and the templates no longer advertise it. The URI string itself
    /// survives as the artifact's name — this pins that only the *serving* went away.
    #[tokio::test]
    async fn the_cas_resource_route_is_gone() {
        let h = media_handler(Arc::new(SyncArtifacts(vec![])));
        let hex = with_text_artifact(&h, b"still reachable by tool");
        let uri = format!("kaibo://cas/{hex}");

        let err = read_kaibo_resource_with_config(
            &uri,
            &[],
            &Config::builtin(),
            &[],
            None,
            false,
            Vec::new(),
            false,
            crate::config::CasMode::Memory,
            None,
        )
        .expect_err("the CAS resource route no longer exists");
        // Recognized, not served: a host with a cached template gets told where the
        // bytes went, with the digest it already has as the argument to use.
        assert!(
            err.message.contains("read_cas") && err.message.contains(&hex),
            "the removal must hand a stale caller its migration: {}",
            err.message
        );
        assert!(
            !err.message.to_lowercase().contains("unknown resource"),
            "and not the bare unknown-resource message: {}",
            err.message
        );

        assert!(
            // The prefix, not a bare "cas" — `kaibo://prompts/{cast}` contains that
            // substring and is a template we still very much serve.
            !kaibo_resource_templates()
                .iter()
                .any(|t| t.uri_template.starts_with(crate::cas::CAS_URI_PREFIX)),
            "and no template advertises it, got {:?}",
            kaibo_resource_templates()
                .iter()
                .map(|t| t.uri_template.clone())
                .collect::<Vec<_>>()
        );

        // The bytes are still reachable — through the tool.
        read(&h, &hex, None, None).await;
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

            [backends.sd]                                      # media backend for `generate`
            kind = "stability"
            key_optional = true

            [casts.artist]                                     # image slot → `generate`
            image    = "sd/core"
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
                // `generate`'s call-time gate is the image slot's presence — the same
                // predicate the enum rule uses, checked here through the cast itself.
                "generate" => cast.slot(crate::config::ModelRole::Image).is_some(),
                other => panic!("unmapped cast-taking tool `{other}` — add its gate here"),
            }
        };

        let mut checked = 0;
        for &(tools, _, _) in CAST_ENUM_RULES {
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

    /// Renders every BUILT-IN cast unusable, so a staffing fixture's roster is exactly
    /// the casts it declares.
    ///
    /// The built-in registry merges under every config, and a built-in cast counts as
    /// usable the moment its backend resolves a credential — from the environment *or*
    /// from a key file on disk. So the keyed backends get an `api_key_file` that cannot
    /// exist (mirroring `config.rs`'s `NO_KEY_FILES`, whose comment records why: Amy
    /// keeps real key files, so a naive fixture passes on her box and fails in CI). The
    /// keyless `openai-local` needs the extra `key_optional = false`: without a required
    /// credential it resolves to a *placeholder* and stays `LocalUnverified`, which counts
    /// as usable — and that is what kept `explore` looking ungated even under an empty
    /// environment, since the built-in local casts all carry explorer slots.
    const NO_BUILTIN_CASTS: &str = r#"
        [backends.anthropic]
        api_key_file = "/nonexistent-kaibo-test/anthropic"
        [backends.deepseek]
        api_key_file = "/nonexistent-kaibo-test/deepseek"
        [backends.gemini]
        api_key_file = "/nonexistent-kaibo-test/gemini"
        [backends.openrouter]
        api_key_file = "/nonexistent-kaibo-test/openrouter"
        [backends.openai-local]
        key_optional = false
        api_key_file = "/nonexistent-kaibo-test/openai"
    "#;

    /// A cast fixture with an interactive team and nothing else — so `explore` has an
    /// explorer, `consult`/`oneshot` have an interactive synth, and NOTHING can staff
    /// `batch_submit` or `deliberate` (both need an offline synth lane).
    /// `NO_BUILTIN_CASTS` followed by `rest`. A plain concatenation rather than
    /// `format!`, because these fixtures contain TOML inline tables — every `{` and `}`
    /// in a slot like `synth = { backend = "gem", ... }` would have to be doubled to
    /// survive a format string, which is a silent trap for the next fixture.
    fn with_no_builtin_casts(rest: &str) -> String {
        format!("{NO_BUILTIN_CASTS}\n{rest}")
    }

    fn only_interactive() -> String {
        with_no_builtin_casts(
            r#"
        [backends.gem]
        kind = "gemini"
        key_optional = true

        [casts.inter]
        explorer = "gem/lite"
        synth    = "gem/flash"

        [server]
        cast = "inter"
    "#,
        )
    }

    /// The staffing gate, `deliberate`'s case — the bug this whole mechanism exists to
    /// fix. `deliberate` needs an offline synth PLUS an explorer; a config carrying only
    /// interactive casts can't staff one, so the tool must not be advertised at all
    /// rather than shipping with an empty `cast` enum and failing at call time.
    #[test]
    fn deliberate_is_not_advertised_when_no_cast_can_staff_it() {
        let h = hermetic_handler_from_toml(&only_interactive());
        assert!(
            !h.advertised_tools().iter().any(|t| t == "deliberate"),
            "deliberate must be dropped when no configured cast pairs an explorer with an \
             offline synth — advertising it costs resident tokens for a tool that can only \
             fail. Advertised: {:?}",
            h.advertised_tools()
        );
    }

    /// The other half: a cast that CAN staff it keeps the tool. Without this, "drop the
    /// route" could be satisfied by dropping it unconditionally.
    #[test]
    fn deliberate_is_advertised_when_a_cast_can_staff_it() {
        let h = hermetic_handler_from_toml(&with_no_builtin_casts(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.inter]
            explorer = "gem/lite"
            synth    = "gem/flash"

            [casts.deep]                                   # explorer + OFFLINE synth
            explorer = "gem/lite"
            synth    = { backend = "gem", id = "pro", lane = "batch" }

            [server]
            cast = "inter"
        "#,
        ));
        assert!(
            h.advertised_tools().iter().any(|t| t == "deliberate"),
            "deliberate must survive when a cast pairs an explorer with an offline synth. \
             Advertised: {:?}",
            h.advertised_tools()
        );
    }

    /// Generalized: the same rule covers `batch_submit`. A config with no batch-lane cast
    /// must not advertise the batch submit verb.
    #[test]
    fn batch_submit_is_not_advertised_without_a_batch_cast() {
        let h = hermetic_handler_from_toml(&only_interactive());
        assert!(
            !h.advertised_tools().iter().any(|t| t == "batch_submit"),
            "batch_submit must be dropped when no cast runs a synth on the batch lane. \
             Advertised: {:?}",
            h.advertised_tools()
        );
    }

    /// And `explore`: a synth-only fixture has no explorer slot anywhere, so the sweep
    /// tool cannot run.
    #[test]
    fn explore_is_not_advertised_without_an_explorer_slot() {
        let h = hermetic_handler_from_toml(&with_no_builtin_casts(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.synth_only]
            synth = "gem/flash"

            [server]
            cast = "synth_only"
        "#,
        ));
        let tools = h.advertised_tools();
        assert!(
            !tools.iter().any(|t| t == "explore"),
            "explore must be dropped when no cast carries an explorer slot. Advertised: {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t == "consult"),
            "consult must SURVIVE here — its interactive synth can staff it, and the driver \
             carries its own explorer. Advertised: {tools:?}"
        );
    }

    /// The collect verbs are shared by three producers (`consult_submit`, `batch_submit`,
    /// `deliberate`), so they must key off *effective* liveness, not the raw `--no-*`
    /// flags: with batch and deliberate unstaffable but consult live, the job verbs stay
    /// (consult_submit still mints `job-N` handles).
    #[test]
    fn job_verbs_survive_on_the_one_staffed_producer() {
        let h = hermetic_handler_from_toml(&only_interactive());
        let tools = h.advertised_tools();
        for verb in ["job_get", "job_cancel", "job_list", "job_wait"] {
            assert!(
                tools.iter().any(|t| t == verb),
                "{verb} must survive while consult_submit can still produce handles. \
                 Advertised: {tools:?}"
            );
        }
    }

    /// ...and drop when NO producer can be staffed. A synth-only cast with `--no-consult`
    /// leaves nothing that can mint a handle, so the collect verbs are dead weight.
    #[test]
    fn job_verbs_drop_when_no_producer_can_be_staffed() {
        let h = hermetic_handler_from_toml(&with_no_builtin_casts(
            r#"
            [backends.gem]
            kind = "gemini"
            key_optional = true

            [casts.synth_only]
            synth = "gem/flash"

            [server]
            cast = "synth_only"

            [server.tools]
            consult = false
        "#,
        ));
        let tools = h.advertised_tools();
        for verb in ["job_get", "job_cancel", "job_list", "job_wait"] {
            assert!(
                !tools.iter().any(|t| t == verb),
                "{verb} must drop when no producer is live: consult is flag-off, and neither \
                 batch_submit nor deliberate can be staffed. Advertised: {tools:?}"
            );
        }
    }

    /// A tool that takes NO cast is never touched by the staffing gate — `run_kaish` runs
    /// the shell with no model at all, so no cast roster can make it unusable.
    #[test]
    fn castless_tools_are_untouched_by_the_staffing_gate() {
        let h = hermetic_handler_from_toml(&only_interactive());
        let tools = h.advertised_tools();
        for tool in ["run_kaish", "list_models"] {
            assert!(
                tools.iter().any(|t| t == tool),
                "{tool} takes no cast — the staffing gate must not reach it. \
                 Advertised: {tools:?}"
            );
        }
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
            .flat_map(|(tools, _, _)| tools.iter().copied())
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
                list_models: true,
                generate: false,
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

    /// Batch-handle persistence + recovery across a simulated restart. With a durable store
    /// injected, `batch_submit` records the `backend/provider-id` handle; after a restart
    /// (a fresh handler + store reopened on the same db, and a provider whose live list is
    /// empty) `job_list` surfaces it under the recovered-handles section — the orphan-
    /// recovery the design doc commits to, now durable rather than session-only.
    #[tokio::test]
    async fn batch_submit_persists_the_handle_and_it_recovers_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let cap = std::num::NonZeroUsize::new(8).unwrap();

        let store = crate::store::SessionStore::open(&db, cap, &[])
            .await
            .unwrap();
        // Provider list is EMPTY, so the store's recovered section is the *only* way the
        // handle appears — exactly the after-restart / lost-handle case.
        let scripted = Arc::new(
            crate::batch::ScriptedBatch::new("msgbatch_r", vec![]).with_listing(vec![], false),
        );
        let h = handler_from_toml(BATCH_CASTS_TOML)
            .with_batch_providers(Arc::new(crate::batch::ScriptedBatchProviders(
                scripted.clone(),
            )))
            .with_session_store(store.clone());

        h.batch_submit(Parameters(BatchSubmitInput {
            prompts: vec!["q".into()],
            attach: vec![],
            cast: Some("mybatch".into()),
            model: None,
            backend: None,
        }))
        .await
        .expect("scripted batch_submit succeeds");

        // Recorded durably (backend `gem`, the scripted id, label = the synth model).
        assert!(
            store
                .get_batch("gem", "msgbatch_r")
                .await
                .unwrap()
                .is_some(),
            "batch_submit must persist the handle when the store is enabled"
        );

        // Simulated restart: a brand-new handler + store reopened on the same db file, and a
        // provider whose live list is still empty.
        let store2 = crate::store::SessionStore::open(&db, cap, &[])
            .await
            .unwrap();
        let empty = Arc::new(
            crate::batch::ScriptedBatch::new("unused", vec![]).with_listing(vec![], false),
        );
        let h2 = handler_from_toml(BATCH_CASTS_TOML)
            .with_batch_providers(Arc::new(crate::batch::ScriptedBatchProviders(empty)))
            .with_session_store(store2);

        let listing = result_text(
            h2.job_list(Parameters(ListInput {
                backend: None,
                all: false,
            }))
            .await
            .expect("job_list succeeds"),
        );
        assert!(
            listing.contains("Recovered batch handles"),
            "the recovered-handles section must appear after a restart:\n{listing}"
        );
        assert!(
            listing.contains("gem/msgbatch_r"),
            "the recovered handle must surface after a restart:\n{listing}"
        );
        assert!(
            listing.contains("some-pro"),
            "the recorded label (synth model) must show on the recovered handle:\n{listing}"
        );
    }

    /// Startup loudness + the invariant amendment: `main` opens the store with the handler's
    /// resolved allowed set, and a state db that resolves inside an allowed project tree is
    /// refused loudly — never silently accepted onto a model-reachable path. This is the
    /// containment feeding `main` relies on, and the reason an open failure is a hard startup
    /// error (crash over silent fallback), not a quiet drop to memory.
    #[tokio::test]
    async fn persistence_store_open_refuses_a_state_db_inside_an_allowed_tree() {
        let proj = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(proj.path().to_path_buf());
        let h = KaiboHandler::new(config).expect("handler builds");
        let allowed = h.allowed_set();
        let allowed_refs: Vec<&Path> = allowed.iter().map(PathBuf::as_path).collect();

        let inside = proj.path().join("state.db");
        match crate::store::SessionStore::open(
            &inside,
            std::num::NonZeroUsize::new(8).unwrap(),
            &allowed_refs,
        )
        .await
        {
            Err(crate::store::StoreError::PathInAllowedTree(_)) => {}
            Err(other) => panic!("wrong error kind: {other:?}"),
            Ok(_) => panic!("a state db inside an allowed project tree must be refused"),
        }
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

        // The dossier build's real explorer spend — the batch lane must surface it in
        // the ack rather than drop it (its synth cost lands later on the batch result).
        let dossier_usage = Usage {
            input_tokens: 200,
            output_tokens: 20,
            total_tokens: 220,
            ..Usage::new()
        };
        let out = h
            .deliberate_batch(
                &cast,
                Some("gem/some-lite"),
                "Is the retry safe?",
                "DOSSIER: src/x.rs:1 fn retry",
                &[],
                "offline-synth-system",
                dossier_usage,
                Some(&KeptDossier {
                    digest: "cd".repeat(32),
                    bytes: 29,
                    path: None,
                    origin: crate::server::dossier::Origin::Built,
                }),
            )
            .await
            .expect("scripted deliberate_batch succeeds");

        let ack = result_text(out);
        assert!(
            ack.contains("gem/msgbatch_d"),
            "the reply carries the durable batch handle"
        );
        assert!(
            ack.contains(&format!("kaibo://cas/{}", "cd".repeat(32))),
            "the ack names the kept dossier's address: {ack}"
        );
        assert!(
            ack.contains("tokens · 200 in · 20 out"),
            "the ack surfaces the synchronous dossier-build cost: {ack}"
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

    /// The reuse road, driven end to end through the real handler: a stored dossier goes in
    /// as `dossier`, NO explorer runs, and the offline synth receives exactly the stored
    /// bytes.
    ///
    /// The lane tests drive `deliberate_batch` directly, so they cannot see the handler's
    /// own wiring — the branch that skips stage 1, what it hands the lane, or whether the
    /// ack still claims an explorer ran. This is that wiring (gap named by the DeepSeek
    /// cross-family review, 2026-08-07).
    #[tokio::test]
    async fn the_reuse_road_runs_the_synth_over_the_stored_dossier_with_no_explorer() {
        let scripted = Arc::new(crate::batch::ScriptedBatch::new("msgbatch_reuse", vec![]));
        let h = handler_from_toml(BATCH_CASTS_TOML).with_batch_providers(Arc::new(
            crate::batch::ScriptedBatchProviders(scripted.clone()),
        ));

        // A dossier already in the store, put there the way an earlier call would have.
        let text = "DOSSIER\nsrc/x.rs:1 fn retry\nsrc/x.rs:9 unbounded backoff\n";
        let kept = dossier::keep_dossier(
            h.media_store(),
            "an earlier question",
            text,
            "mydeliberate",
            "gem/some-lite",
        )
        .expect("the test handler has a live memory CAS");

        let out = h
            .deliberate_call(
                DeliberateInput {
                    question: "does the second synth see the same evidence?".into(),
                    attach: vec![],
                    path: None,
                    cast: Some("mydeliberate".into()),
                    explorer_model: None,
                    explorer_backend: None,
                    synth_model: None,
                    synth_backend: None,
                    explorer_max_turns: None,
                    dossier: Some(kept.digest.clone()),
                },
                Arc::new(NullSink),
            )
            .await
            .expect("a reuse call succeeds");

        let ack = result_text(out);
        assert!(
            ack.contains("Dossier reused") && ack.contains("no explorer ran"),
            "the ack must not claim a sweep that never happened: {ack}"
        );
        assert!(
            ack.contains(&format!("kaibo://cas/{}", kept.digest)),
            "the ack names the dossier it reasoned over: {ack}"
        );

        // The stored bytes are what reached the synth — verbatim, beside the new question.
        // Anything less and reuse is not reuse.
        let submits = scripted.submits();
        assert_eq!(submits.len(), 1, "one deliberation was submitted");
        let (_system, attach, items) = &submits[0];
        assert!(
            attach.is_empty(),
            "no sweep ran, so nothing routed images into the submit"
        );
        assert_eq!(items.len(), 1);
        assert!(
            items[0].prompt.contains(text)
                && items[0]
                    .prompt
                    .contains("does the second synth see the same evidence?"),
            "the reused dossier AND the new question must both reach the synth: {}",
            items[0].prompt
        );
    }

    /// A reuse call carrying explorer arguments is refused by the handler, and a dossier
    /// kaibo does not hold is an error — never a silent fall back to sweeping, since the
    /// caller asked to reason over specific evidence.
    #[tokio::test]
    async fn the_handler_refuses_a_reuse_call_it_cannot_serve_as_asked() {
        let h = handler_from_toml(BATCH_CASTS_TOML);
        let reuse = |attach: Vec<String>, digest: &str| DeliberateInput {
            question: "q".into(),
            attach,
            path: None,
            cast: Some("mydeliberate".into()),
            explorer_model: None,
            explorer_backend: None,
            synth_model: None,
            synth_backend: None,
            explorer_max_turns: None,
            dossier: Some(digest.to_string()),
        };

        let err = h
            .deliberate_call(
                reuse(vec!["src/x.rs".into()], &"aa".repeat(32)),
                Arc::new(NullSink),
            )
            .await
            .expect_err("`attach` on a reuse call is refused");
        assert!(
            err.message.contains("`attach`"),
            "the refusal names the inert argument: {}",
            err.message
        );

        let err = h
            .deliberate_call(reuse(vec![], &"aa".repeat(32)), Arc::new(NullSink))
            .await
            .expect_err("an unknown digest is refused");
        assert!(
            err.message.contains("never stored here"),
            "the refusal says the dossier is not held: {}",
            err.message
        );
    }

    /// A batch deliberation runs for hours, so the ack that named its dossier is long gone
    /// (or the server restarted) by the time anyone collects it. The dossier's address
    /// must therefore be recoverable from kaibo's own durable record: `deliberate`'s batch
    /// lane persists its handle — which it did not do at all before dossiers were kept —
    /// with a label naming the synth AND the dossier, and `job_list` surfaces both after a
    /// restart. Without this, keeping the dossier would still lose the way back to it on
    /// the one lane where losing it is likely.
    #[tokio::test]
    async fn a_deliberate_batch_handle_recovers_with_its_dossier_address() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let cap = std::num::NonZeroUsize::new(8).unwrap();
        let digest = "ef".repeat(32);

        let store = crate::store::SessionStore::open(&db, cap, &[])
            .await
            .unwrap();
        // Empty provider listing, so the recovered section is the only way back.
        let scripted = Arc::new(
            crate::batch::ScriptedBatch::new("msgbatch_dl", vec![]).with_listing(vec![], false),
        );
        let h = handler_from_toml(BATCH_CASTS_TOML)
            .with_batch_providers(Arc::new(crate::batch::ScriptedBatchProviders(
                scripted.clone(),
            )))
            .with_session_store(store.clone());
        let cast = h.config.resolve_cast("mydeliberate").unwrap().clone();

        h.deliberate_batch(
            &cast,
            Some("gem/some-lite"),
            "Is the retry safe?",
            "DOSSIER: src/x.rs:1 fn retry",
            &[],
            "offline-synth-system",
            Usage::new(),
            Some(&KeptDossier {
                digest: digest.clone(),
                bytes: 29,
                path: None,
                origin: crate::server::dossier::Origin::Built,
            }),
        )
        .await
        .expect("scripted deliberate_batch succeeds");

        // Simulated restart: a new handler and a store reopened on the same db file.
        let store2 = crate::store::SessionStore::open(&db, cap, &[])
            .await
            .unwrap();
        let empty = Arc::new(
            crate::batch::ScriptedBatch::new("unused", vec![]).with_listing(vec![], false),
        );
        let h2 = handler_from_toml(BATCH_CASTS_TOML)
            .with_batch_providers(Arc::new(crate::batch::ScriptedBatchProviders(empty)))
            .with_session_store(store2);
        let listing = result_text(
            h2.job_list(Parameters(ListInput {
                backend: None,
                all: false,
            }))
            .await
            .expect("job_list succeeds"),
        );

        assert!(
            listing.contains("gem/msgbatch_dl"),
            "a deliberate batch handle must survive a restart:\n{listing}"
        );
        assert!(
            listing.contains(&format!("kaibo://cas/{digest}")),
            "the recovered handle must name the dossier it was built from:\n{listing}"
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
        assert!(progress_token(&RequestMetaObject::default()).is_none());
    }

    /// A token in `_meta` is the opt-in — we surface it so a reporter can be built.
    #[test]
    fn a_progress_token_in_meta_is_surfaced() {
        let token = ProgressToken(NumberOrString::Number(7));
        let meta = RequestMetaObject::with_progress_token(token.clone());
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
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
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
                .any(|t| t.uri_template == BUILTIN_URI_TEMPLATE),
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
        let ContentBlock::Text(t) = &result.messages[0].content else {
            panic!("configure prompt must be a text message");
        };
        let text = t.text.as_str();
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
        let ContentBlock::Text(t) = &result.messages[0].content else {
            panic!("configure prompt must be a text message");
        };
        let text = t.text.as_str();
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

    /// Host-agent sandboxes are a setup concern outside kaibo's inner read-only shell.
    /// The configure guidance must name the access kaibo actually needs and the option to
    /// split durable state per client/host-agent, especially for Codex's stricter default.
    #[test]
    fn configure_prompt_covers_host_sandbox_state_and_cas_access() {
        let result =
            kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, None).expect("configure must resolve");
        let ContentBlock::Text(t) = &result.messages[0].content else {
            panic!("configure prompt must be a text message");
        };
        let text = t.text.as_str();
        for needle in [
            "network_access",
            "writable_roots",
            "$XDG_STATE_HOME/kaibo/state.db",
            "$XDG_DATA_HOME/kaibo/cas",
            "per-client stores",
            "Codex",
            "Claude Code",
        ] {
            assert!(
                text.contains(needle),
                "configure prompt should mention host-sandbox setup fact {needle:?}; body:\n{text}"
            );
        }
    }

    /// A supplied `goal` is woven into the message so the agent tailors the roster;
    /// a blank one is treated as absent (no dangling "goal:" line).
    #[test]
    fn configure_prompt_weaves_in_a_goal() {
        let args = json!({ "goal": "a local-only privacy cast" });
        let with_goal = kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, args.as_object())
            .expect("configure must resolve");
        let ContentBlock::Text(t) = &with_goal.messages[0].content else {
            panic!("expected text");
        };
        let text = t.text.as_str();
        assert!(
            text.contains("a local-only privacy cast"),
            "a supplied goal must appear in the prompt; body:\n{text}"
        );

        let blank = json!({ "goal": "   " });
        let without = kaibo_prompt_messages(CONFIGURE_PROMPT_NAME, blank.as_object())
            .expect("configure must resolve");
        let ContentBlock::Text(t) = &without.messages[0].content else {
            panic!("expected text");
        };
        let text = t.text.as_str();
        assert!(
            !text.contains("My goal for this setup:"),
            "a blank goal must not append an empty goal line; body:\n{text}"
        );
    }

    /// `kaibo configure` (CLI) must point a caller at the plain subcommands it can
    /// actually reach, never the MCP resource URIs the whole command exists to work
    /// around — but it must still carry the same roster-design substance (the shared
    /// [`CONFIGURE_STEPS_CORE`]) as the MCP prompt, so a CLI-only reader isn't shorted.
    #[test]
    fn configure_prompt_text_cli_points_at_subcommands_not_mcp_resources() {
        let text = configure_prompt_text_cli(None);
        for needle in [
            "kaibo example-config", // read the annotated template
            "kaibo config",         // and the resolved live state
            "config.toml",          // write target
            "api_key_env",          // keys-by-reference, not inline
            "both within it",       // shared roster-design substance
            "output ceiling",       // synth max_tokens sizing rides the shared core
        ] {
            assert!(
                text.contains(needle),
                "kaibo configure should mention {needle:?}; body:\n{text}"
            );
        }
        for unreachable in [CONFIG_EXAMPLE_URI, CONFIG_URI] {
            assert!(
                !text.contains(unreachable),
                "kaibo configure must not point a CLI-only caller at an MCP resource \
                 URI it may not be able to reach ({unreachable:?}); body:\n{text}"
            );
        }
    }

    /// Same goal-weaving contract as the MCP prompt ([`configure_prompt_weaves_in_a_goal`]):
    /// a supplied goal appears verbatim, and blank/whitespace reads as absent. Also covers
    /// an *empty* string (`Some("")`, not just whitespace) — clap hands `run_configure` a
    /// raw `Option<String>` straight from `argv` with no upstream trim/filter (unlike the
    /// MCP path's `kaibo_prompt_messages`), so `append_configure_goal` is the only gate;
    /// this pins that it actually catches the empty case, not just whitespace.
    #[test]
    fn configure_prompt_text_cli_weaves_in_a_goal_and_filters_blank_or_empty() {
        let with_goal = configure_prompt_text_cli(Some("a local-only privacy cast"));
        assert!(
            with_goal.contains("a local-only privacy cast"),
            "a supplied goal must appear in the CLI text; body:\n{with_goal}"
        );

        for blank in [None, Some(""), Some("   ")] {
            let text = configure_prompt_text_cli(blank);
            assert!(
                !text.contains("My goal for this setup:"),
                "a blank/empty/absent goal ({blank:?}) must not append an empty goal \
                 line; body:\n{text}"
            );
        }
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
        let result = read_kaibo_resource_with_config(
            uri,
            schemas,
            &config,
            &allowed,
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        )
        .expect("known uri must read");
        let result = match result {
            ReadResourceResponse::Complete(r) => r,
            other => panic!("expected complete result, got {other:?}"),
        };
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    /// The sandbox resource is where the exit-code taxonomy is stated in full, so unlike
    /// the terse addendum it *does* carry `126` — correctly, as the operator-disabled
    /// builtin case. What it must also carry is the refusal's real signature, since a
    /// refused write and an ordinary failure share exit `1` and only the message
    /// separates them.
    #[test]
    fn reads_the_sandbox_doc_with_the_idioms_and_codes() {
        let text = read_text(SANDBOX_URI, &[]);
        for needle in [
            "cat -n",
            "grep",
            "read-only",
            "126",
            "124",
            "127",
            "permission denied: filesystem is read-only",
        ] {
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
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
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
            "permission denied: filesystem is read-only", // how a refusal names itself
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
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
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
            .map(|t| t.uri_template)
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
            false,
            crate::config::CasMode::Memory,
            None,
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
                false,
                crate::config::CasMode::Memory,
                None,
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
                vec![],
                false,
                crate::config::CasMode::Memory,
                None,
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
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
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
            false,
            crate::config::CasMode::Memory,
            None,
        )
        .expect("example resource must be readable");
        let result = match result {
            ReadResourceResponse::Complete(r) => r,
            other => panic!("expected complete result, got {other:?}"),
        };
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

    // --- kaibo://config/guide resource tests ----------------------------------

    /// The embedded configuration manual is listed and readable. It carries the detail
    /// the template deliberately no longer inlines, so an agent configuring kaibo over
    /// MCP (no access to this repo's `docs/`) can still reach it.
    #[test]
    fn config_guide_resource_is_listed_and_readable() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
        assert!(
            uris.iter().any(|u| u == CONFIG_GUIDE_URI),
            "kaibo_resources() must list kaibo://config/guide, got {uris:?}"
        );

        let body = render_resource(CONFIG_GUIDE_URI, &[]).expect("guide must render");
        assert!(
            body.starts_with("# kaibo configuration"),
            "guide must be docs/config.md verbatim:\n{}",
            &body[..body.len().min(200)]
        );
    }

    /// The pointer the trimmed template makes — "full table: kaibo://config/guide,
    /// 'Tool gating'" — must actually land somewhere. This is the drift guard for
    /// moving that content out of the example: delete or rename the section and the
    /// template becomes a dead reference, so fail here instead.
    #[test]
    fn config_guide_carries_the_tool_gating_section_the_template_points_at() {
        let guide = render_resource(CONFIG_GUIDE_URI, &[]).expect("guide must render");
        assert!(
            guide.contains("## Tool gating"),
            "docs/config.md must keep the section config.example.toml points at"
        );
        assert!(
            CONFIG_EXAMPLE_TOML.contains(CONFIG_GUIDE_URI),
            "the template must point at the guide it defers its detail to"
        );
        // Each cast-gated tool is named where an operator would look up why it vanished.
        for tool in ["consult", "explore", "batch_submit", "deliberate"] {
            assert!(
                guide.contains(tool),
                "the gating section must account for `{tool}`"
            );
        }
    }

    // --- kaibo://config resource tests ---------------------------------------

    /// The config resource must appear in the listing with the correct URI and a
    /// useful description. Failing until `kaibo://config` is added to
    /// `kaibo_resources()`.
    #[test]
    fn config_resource_is_listed() {
        let uris: Vec<String> = kaibo_resources().into_iter().map(|r| r.uri).collect();
        assert!(
            uris.iter().any(|u| u == CONFIG_URI),
            "kaibo_resources() must list kaibo://config, got {uris:?}"
        );
        // The resource entry for the config must also have a description.
        let config_res = kaibo_resources()
            .into_iter()
            .find(|r| r.uri == CONFIG_URI)
            .expect("config resource must be listed");
        assert!(
            config_res.description.is_some(),
            "kaibo://config resource must have a description"
        );
    }

    /// `read_kaibo_resource` extended: kaibo://config must be readable via the
    /// handler-level path (which threads config + allowed_set through).
    #[test]
    fn read_kaibo_config_resource_is_readable() {
        let config = Config::builtin();
        let allowed = handler().allowed_set();
        let body_str = render_config_resource(
            &config,
            &allowed,
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
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
            false,
            crate::config::CasMode::Memory,
            None,
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
