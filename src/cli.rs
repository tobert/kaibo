//! The CLI front door — `kaibo consult …`, `kaibo config`, and (from `main`) the
//! implicit/`serve` MCP server.
//!
//! kaibo is an MCP server first; the CLI is the *same* read-only codebase
//! consultation, reachable without an MCP client (CLI-first agents, scripts, CI, a
//! human at a terminal). It runs the identical resolution glue the server does —
//! the shared [`Resolver`](crate::server::Resolver) — so a `--session` started over
//! MCP continues on the CLI and back. Two contracts a script can rely on:
//!
//! - **stdout is the answer, stderr is everything else.** The answer (with the same
//!   provenance footer the MCP tool returns) goes to stdout; progress beats, logs, and
//!   any non-fatal warnings go to stderr (via
//!   [`TerminalSink`](crate::progress::TerminalSink)). `--json` swaps stdout for a
//!   structured envelope (answer + provenance + usage + warnings) — and the `answer`
//!   field is the model's raw words, never kaibo's injected notices.
//! - **exit codes have teeth.** `0` = an answer; `2` = a **usage** error — a bad
//!   argument, an unknown or wrong-for-the-tool cast, an image on a vision-blind cast
//!   (also clap's own arg-parse errors, and config load); `3` = a **setup/containment**
//!   rejection — a path or attachment outside the allowed set, a missing/unbuildable
//!   provider key; `4` = the work ran and failed at runtime (a provider/model-loop
//!   failure, or a `kaish` worker infra crash). So an agent branches on the code without
//!   parsing prose. (An arg-parse error is clap's: usage on stderr, exit 2, nothing on
//!   stdout — the envelope is guaranteed only once args parse. And `kaibo kaish` passes
//!   through kaish's own exit code on a normal run — 0/126 blocked/124 timed out.)
//!
//! `--help` is model-facing text: an agent reads it the way an MCP client reads a
//! tool description, so the top-level `about` front-loads what kaibo is and every
//! flag doc earns its line (the "Writing for models" discipline).

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use rig_core::completion::Usage;
use rmcp::ErrorData as McpError;

use crate::batch::{BatchItem, BatchPoll};
use crate::config::{Config, ModelRole, ToolDisables};
use crate::consult::{
    batch_system_prompt, consult, explore_with, oneshot as run_oneshot_engine, ConsultConfig,
    ConsultOutput, ExploreConfig, ModelCaps, PhaseContext,
};
use crate::progress::{NullSink, TerminalSink};
use crate::sandbox::KaishWorker;
use crate::server::{
    batch_within_window, consultation_failure_text, is_local_handle, now_epoch_secs,
    parse_local_handle, render_config_resource, render_local_cancel, render_local_job,
    render_local_list, with_provenance, Resolver, BATCH_RECENCY_WINDOW_SECS,
};
use crate::session::{SessionStore as MemSessionStore, Sessions};
use crate::store::{CancelLocalOutcome, LocalJob, LocalJobStatus};

/// Exit codes, distinct so an agent caller branches without parsing prose.
pub const EXIT_OK: i32 = 0;
/// A usage or config-load error (also clap's own arg-error code): a bad argument, an
/// unknown or wrong-for-the-tool cast, bad model-override args, an image on a
/// vision-blind cast.
pub const EXIT_USAGE: i32 = 2;
/// A setup/containment rejection: a `--path`/`--root`/attachment outside the allowed
/// boundary, or a missing/unbuildable provider key.
pub const EXIT_SETUP: i32 = 3;
/// The work ran but failed at runtime: a consultation failure (provider overload,
/// model-loop error, timeout), or a `kaish` worker infra failure (kernel crash/panic,
/// worker channel closed) — distinct from a pre-flight rejection above. Note a normal
/// kaish script/blocked/timeout outcome is not this: it returns kaish's own exit code.
pub const EXIT_CONSULT_FAILURE: i32 = 4;

/// kaibo (解剖) — read-only codebase consultation from a model outside your own
/// family. Ask a question; a capable model (DeepSeek, Gemini, Anthropic, OpenRouter,
/// or local — pick with `--cast`) reads the project READ-ONLY and answers with
/// `file:line` citations, never modifying anything. Bare `kaibo` is the MCP server
/// (stdio); `kaibo consult` is the one-shot CLI; `kaibo config` prints the resolved
/// configuration.
#[derive(Parser, Debug)]
#[command(name = "kaibo", version)]
pub struct Cli {
    /// The shared flags (config discovery, containment, cast, house-rules,
    /// persistence). Defined once here as clap `global` args, so they work **before or
    /// after** the subcommand — `kaibo --cast x consult …` and `kaibo consult … --cast
    /// x` both reach the consult, and `kaibo --cast x` alone reaches the implicit serve.
    #[command(flatten)]
    pub common: CommonArgs,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// The implicit-serve tool gates: a bare `kaibo` (no subcommand) runs the MCP
    /// server, so every existing client config keeps working unchanged. Serve-only —
    /// they're read only on the serve path; the subcommands ignore them. (We don't use
    /// `args_conflicts_with_subcommands` because it would also fence the shared globals
    /// off a subcommand, defeating `kaibo --cast x consult …`.)
    #[command(flatten)]
    pub gates: ServeGates,
}

#[derive(Subcommand, Debug)]
// The parsed CLI lives for one `main` dispatch, so the size gap between the arg
// variants is irrelevant — and clap's derive can't parse a `Box<Args>` variant.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Run the MCP server on stdio (the explicit form of a bare `kaibo`).
    Serve(ServeGates),
    /// Ask one read-only consultation question; the cited answer prints to stdout.
    Consult(ConsultArgs),
    /// A toolless second opinion: no codebase access, the model answers from your prompt
    /// (plus piped stdin and `--attach` files) and its own knowledge.
    Oneshot(OneshotArgs),
    /// Survey the codebase and print a cited report (the evidence half of consult).
    Explore(ExploreArgs),
    /// Run one kaish (sh-like) command against the read-only project and print its output.
    Kaish(KaishArgs),
    /// Provider batch lanes — submit a fan-out, get results, list live/recovered handles.
    Batch(BatchArgs),
    /// Print the resolved runtime configuration (the `kaibo://config` document).
    Config,
}

/// Flags shared by every front door: config discovery, the containment boundary, the
/// default cast, house-rules files, and the persistence store.
#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// Path to config.toml. Overrides $KAIBO_CONFIG; default is
    /// $XDG_CONFIG_HOME/kaibo/config.toml (absent → built-in defaults).
    #[arg(long, value_name = "FILE", global = true)]
    pub config: Option<PathBuf>,

    /// Default project root, and an allowed tree. A per-call `--path` must resolve to
    /// at-or-under `--root` or an `--allow-path` tree. With neither, the allowed set
    /// (and inferred default root) is the invocation cwd — so run kaibo from the
    /// workspace and you configure nothing.
    #[arg(long, value_name = "DIR", global = true)]
    pub root: Option<PathBuf>,

    /// Additional allowed path tree. Repeatable. Use `--allow-path /` to lift all
    /// limits. Also settable via KAIBO_ALLOW_PATHS (colon-separated) or
    /// [server] allow_paths; a non-empty set of flags replaces that layer.
    #[arg(long = "allow-path", value_name = "DIR", action = clap::ArgAction::Append, global = true)]
    pub allow_path: Vec<PathBuf>,

    /// Don't follow git worktrees of an allowed repo (by default a sibling worktree is
    /// reachable without an --allow-path). Also KAIBO_NO_FOLLOW_WORKTREES.
    #[arg(long, global = true)]
    pub no_follow_worktrees: bool,

    /// Default cast (model team) when a call omits it: a built-in (anthropic |
    /// deepseek | gemini | openrouter | openai-local, plus aliases) or one from
    /// config.toml. `kaibo config` lists every cast.
    #[arg(long, global = true)]
    pub cast: Option<String>,

    /// Project house-rules file spliced into the consult preamble, resolved relative
    /// to the root and read only if present. Repeatable; defaults to AGENTS.md.
    #[arg(long = "project-context-file", value_name = "FILE", action = clap::ArgAction::Append, global = true)]
    pub project_context_file: Vec<String>,

    /// User house-rules file (e.g. ~/.claude/CLAUDE.md) spliced into the preamble;
    /// read unconditionally (missing is an error). Repeatable.
    #[arg(long = "user-context-file", value_name = "FILE", action = clap::ArgAction::Append, global = true)]
    pub user_context_file: Vec<PathBuf>,

    /// Don't persist sessions or batch handles — run fully in-memory. By default kaibo
    /// keeps a small state db so a `--session` survives a restart and is shared across
    /// front doors. Also KAIBO_NO_PERSISTENCE.
    #[arg(long, global = true)]
    pub no_persistence: bool,

    /// Path to the persistence state db. Overrides KAIBO_STATE_DB / [persistence] path
    /// / the default ($XDG_STATE_HOME/kaibo/state.db).
    #[arg(long = "state-db", value_name = "FILE", global = true)]
    pub state_db: Option<PathBuf>,
}

/// The per-tool `--no-<tool>` gates — serve-only (they only make sense for the
/// long-lived server). The shared flags live on [`Cli::common`] as globals.
#[derive(Args, Debug, Clone, Default)]
pub struct ServeGates {
    /// Don't advertise the `consult` tool.
    #[arg(long)]
    pub no_consult: bool,
    /// Don't advertise the `explore` tool.
    #[arg(long)]
    pub no_explore: bool,
    /// Don't advertise the `deliberate` tool.
    #[arg(long)]
    pub no_deliberate: bool,
    /// Don't advertise the `oneshot` tool.
    #[arg(long)]
    pub no_oneshot: bool,
    /// Don't advertise the `run_kaish` tool.
    #[arg(long)]
    pub no_run_kaish: bool,
    /// Don't advertise `batch_submit`. The shared job verbs stay while `consult` is on.
    #[arg(long)]
    pub no_batch: bool,
}

impl ServeGates {
    /// The `--no-<tool>` flags as a [`ToolDisables`].
    pub fn tool_disables(&self) -> ToolDisables {
        ToolDisables {
            consult: self.no_consult,
            explore: self.no_explore,
            deliberate: self.no_deliberate,
            oneshot: self.no_oneshot,
            run_kaish: self.no_run_kaish,
            batch: self.no_batch,
        }
    }
}

/// `kaibo consult` — one read-only consultation. The shared flags come from
/// [`Cli::common`] (globals); these are the consult-specific ones.
#[derive(Args, Debug)]
pub struct ConsultArgs {
    /// The question to investigate. Say in prose what you did or want to know — kaibo
    /// locates and reads the real, current code itself, so your intent beats a diff.
    pub question: String,

    /// Project to explore. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Workspace file to put in front of the investigation (inlined if small, else read
    /// whole by the model; an image needs a vision-capable cast). Repeatable.
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<String>,

    /// Multi-turn session name: kaibo replays this session's prior turns and records
    /// this one. Shared with the MCP server through the persistent store, so a session
    /// started there continues here. Omit for a stateless one-shot.
    #[arg(long, value_name = "NAME")]
    pub session: Option<String>,

    /// Optional starting evidence — a change/diff summary or pasted source kaibo can't
    /// reach. Trusted: kaibo extends it rather than re-deriving cited spans.
    #[arg(long)]
    pub context: Option<String>,

    /// Override the explorer (investigation) model id (verbatim; pair with
    /// --explorer-backend to also retarget).
    #[arg(long, value_name = "ID")]
    pub explorer_model: Option<String>,
    /// Run the explorer override on this backend (name or alias). Requires --explorer-model.
    #[arg(long, value_name = "BACKEND")]
    pub explorer_backend: Option<String>,
    /// Override the synth (final-answer) model id (pair with --synth-backend to retarget).
    #[arg(long, value_name = "ID")]
    pub synth_model: Option<String>,
    /// Run the synth override on this backend (name or alias). Requires --synth-model.
    #[arg(long, value_name = "BACKEND")]
    pub synth_backend: Option<String>,

    /// Max tool-loop turns per delegated explorer sweep (default 100).
    #[arg(long, value_name = "N")]
    pub explorer_max_turns: Option<usize>,
    /// Max tool-loop turns for the consult driver loop (default 200).
    #[arg(long, value_name = "N")]
    pub synth_max_turns: Option<usize>,

    /// Also print the explorer's aggregated report (under `report` in --json; appended
    /// on a rule below otherwise). Empty when the consult delegated no sweep.
    #[arg(long)]
    pub include_report: bool,

    /// Emit a JSON envelope on stdout (answer + provenance + usage + warnings) instead
    /// of prose, for a script caller. Note: an argument-parse error prints usage to
    /// stderr and exits 2 with nothing on stdout — the JSON envelope is guaranteed only
    /// once the arguments parse.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo oneshot` — a toolless second opinion from a model outside your family. No
/// codebase access: the model answers from your prompt (plus any context piped on
/// stdin, `… < notes.md`) and `--attach`ed files. Pick the answering team with `--cast`.
#[derive(Args, Debug)]
pub struct OneshotArgs {
    /// The prompt to send the model. Context piped on stdin is appended (the
    /// `oneshot "review this" < diff.txt` idiom). No codebase access, so include (or
    /// `--attach`) whatever the answer needs.
    pub prompt: String,

    /// Workspace file to inline as context — kaibo reads it so its bytes never pass
    /// through your context. Prefer whole files; an image needs a vision-capable cast.
    /// Repeatable.
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<String>,

    /// Override the model id (verbatim; pair with --backend to also retarget).
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
    /// Run the `--model` override on this backend (name or alias). Requires --model.
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Emit a JSON envelope on stdout (answer + provenance + usage) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo explore` — a cited survey report (the evidence-gathering half of consult): a
/// model sweeps the project READ-ONLY and returns findings + `file:line` locations +
/// the trail it followed, not a synthesized answer.
#[derive(Args, Debug)]
pub struct ExploreArgs {
    /// What to survey or map. Say in prose what you want charted — kaibo's explorer
    /// locates and reads the real, current code and reports back with citations.
    pub question: String,

    /// Project to explore. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Workspace file central to the survey: the investigator is directed to read it
    /// WHOLE. Text only (it reads through the shell). Repeatable.
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<String>,

    /// Override the explorer model id (verbatim; pair with --explorer-backend).
    #[arg(long, value_name = "ID")]
    pub explorer_model: Option<String>,
    /// Run the explorer override on this backend (name or alias). Requires --explorer-model.
    #[arg(long, value_name = "BACKEND")]
    pub explorer_backend: Option<String>,
    /// Max tool-loop turns for the explorer sweep (default 100).
    #[arg(long, value_name = "N")]
    pub explorer_max_turns: Option<usize>,

    /// Emit a JSON envelope on stdout (report + provenance + usage) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo kaish` — one non-interactive kaish command through the same READ-ONLY sandbox
/// the `run_kaish` MCP tool uses. Scriptable single execution only: no readline, no
/// REPL. The process exits with kaish's own exit code (0 ok, 126 blocked, 124 timed out).
#[derive(Args, Debug)]
pub struct KaishArgs {
    /// The kaish (sh-like) script to run against the read-only project. Required — kaibo
    /// has no interactive shell, so `-c` is the only way in (a missing `-c` is a usage
    /// error, not a prompt). `cat -n FILE`, `grep -rn PATTERN .`, pipes with jq/awk/find.
    #[arg(short = 'c', value_name = "SCRIPT")]
    pub command: Option<String>,

    /// Project to run against. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree. Each call starts fresh at this root.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Emit a JSON object `{stdout, stderr, exit_code}` instead of raw streams (the
    /// process still exits with kaish's exit code).
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch` — the provider batch lanes (offline, max thinking, half price) exactly
/// as the MCP verbs drive them.
#[derive(Args, Debug)]
pub struct BatchArgs {
    #[command(subcommand)]
    pub cmd: BatchCmd,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BatchCmd {
    /// Fan self-contained prompts to a batch cast; prints the durable `backend/id` handle.
    /// With `--local`, enqueue to the state db instead — no provider lane, drained by
    /// `kaibo batch work` (prints a `local/<id>` handle).
    Submit(BatchSubmitArgs),
    /// Drain the local batch queue in the foreground: claim pending `local/<id>` jobs one at
    /// a time and run each item on its cast. Background it with `&` / systemd-run / cron.
    Work(BatchWorkArgs),
    /// Fetch a batch by handle — a progress line while it runs, per-item answers when done.
    /// Handles a `backend/provider-id` provider batch or a `local/<id>` local job.
    Get(BatchGetArgs),
    /// List provider batches (live + store-recovered handles) and local batch jobs.
    List(BatchListArgs),
    /// Cancel a batch by handle — a provider `backend/provider-id`, or a `local/<id>` job.
    Cancel(BatchCancelArgs),
}

/// `kaibo batch submit` — like `oneshot`, no tools/codebase access: each prompt carries
/// its own context (or shared `--attach` files). Needs a batch-capable cast/backend.
#[derive(Args, Debug)]
pub struct BatchSubmitArgs {
    /// The prompts to fan out, one batch item each. At least one required.
    #[arg(required = true, value_name = "PROMPT")]
    pub prompts: Vec<String>,

    /// Workspace file inlined as shared context for every prompt (kaibo reads it so its
    /// bytes never pass through your context). Repeatable.
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<String>,

    /// Override the synth model id — batch a top-tier model. Pair with --backend.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
    /// Run the `--model` override on this backend (must be batch-capable). Requires --model.
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Enqueue to the local batch lane (the state db) instead of a provider batch. Runs on
    /// LOCAL compute (or any cast), no provider batch lane needed; drained by `kaibo batch
    /// work`. Prints a `local/<id>` handle. Any cast works — no batch-capable cast required.
    #[arg(long)]
    pub local: bool,

    /// Emit a JSON object `{handle, cast, model, count}` instead of the bare handle.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch work` — the foreground local-batch worker. Claims pending `local/<id>` jobs
/// one at a time and runs each item on the job's cast (resolved against THIS process's
/// config), writing per-item results as they land. Exits 0 when the queue is empty.
#[derive(Args, Debug)]
pub struct BatchWorkArgs {
    /// After draining, keep polling for new pending jobs every N seconds instead of exiting.
    /// Omit for a single drain-then-exit pass (the cron/systemd-run shape).
    #[arg(long, value_name = "SECS")]
    pub watch: Option<u64>,

    /// Emit a JSON object `{drained}` at the end instead of the prose summary.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch cancel` — stop a batch by handle.
#[derive(Args, Debug)]
pub struct BatchCancelArgs {
    /// The handle to cancel — a provider `backend/provider-id`, or a `local/<id>` job.
    pub handle: String,

    /// Emit a JSON object instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch get` — collect a batch by its `backend/provider-id` handle.
#[derive(Args, Debug)]
pub struct BatchGetArgs {
    /// The `backend/provider-id` handle `batch submit` printed.
    pub handle: String,

    /// Emit a JSON object (status + progress or per-item answers) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch list` — the way back to a batch whose handle you lost.
#[derive(Args, Debug)]
pub struct BatchListArgs {
    /// Which backend (name or alias) to list. Omit to sweep every batch-capable backend.
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Show all batches, including ones older than 24h (trimmed by default).
    #[arg(long)]
    pub all: bool,

    /// Emit a JSON object (entries + recovered handles + per-backend errors) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// Load config for a CLI subcommand and overlay the shared CLI flags. Tool gating
/// stays default-on (the `--no-<tool>` gates are a serve-only concern), so a
/// disabled-tool config never blocks a CLI consult.
fn load_config(common: &CommonArgs) -> anyhow::Result<Config> {
    let config_path = common
        .config
        .clone()
        .or_else(|| std::env::var_os("KAIBO_CONFIG").map(PathBuf::from));
    let mut config = Config::load(config_path)?;
    config.apply_cli(
        common.root.clone(),
        common.cast.clone(),
        ToolDisables::default(),
        common.allow_path.clone(),
        common.no_follow_worktrees,
        common.project_context_file.clone(),
        common.user_context_file.clone(),
        common.no_persistence,
        common.state_db.clone(),
    );
    Ok(config)
}

/// A quiet-by-default stderr tracing subscriber for the CLI: RUST_LOG still wins, but
/// the CLI's own liveness is the [`TerminalSink`] progress channel, so the log floor
/// defaults to `warn` and stays out of the answer stream. Best-effort — a second
/// init (only possible if a caller wired one already) is ignored.
fn init_cli_logging() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter),
        )
        .try_init();
}

/// Print `err` to stderr and return `code` — the shared shape for a **pre-flight**
/// rejection (usage *or* setup/containment), i.e. anything refused before the model
/// runs. Carries the `kind`/`code` its caller classified (it renders both, so the name
/// no longer claims "setup" only). With `--json`, the message rides a structured
/// envelope on stdout so a script parses it uniformly with a success envelope.
fn fail_preflight(json: bool, kind: &str, message: String, code: i32) -> i32 {
    if json {
        println!("{}", error_envelope(kind, &message));
    } else {
        eprintln!("kaibo: {message}");
    }
    code
}

/// Run `kaibo consult`. Returns the process exit code (never panics on an expected
/// failure — a bad cast, a provider outage, and a clean answer all return through
/// here with their own code).
pub async fn run_consult(common: CommonArgs, args: ConsultArgs) -> i32 {
    init_cli_logging();

    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            return fail_preflight(
                args.json,
                "config",
                format!("config error: {e:#}"),
                EXIT_USAGE,
            )
        }
    };
    // The shared resolver computes the allowed set + inferred default root exactly as
    // the server does, so this invocation's cwd joins the boundary the same way.
    let persistence = config.persistence.clone();
    let session_capacity = config.defaults.session_capacity;
    let inline_budget = config.defaults.inline_attach_budget;
    let call_deadline = config.defaults.call_deadline;
    let default_explorer_turns = config.defaults.explorer_max_turns;
    let default_synth_turns = config.defaults.synth_max_turns;
    let sandbox = config.sandbox.clone();
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => {
            return fail_preflight(args.json, "setup", format!("{e:#}"), EXIT_SETUP);
        }
    };

    // Resolution stage — every refusable thing is either a usage (exit 2) or a
    // setup/containment (exit 3) rejection, distinct from a consultation that ran and
    // failed (exit 4). Each call site tags its own class; see `resolve_and_run`.
    let outcome = resolve_and_run(
        &common,
        &args,
        &resolver,
        inline_budget,
        call_deadline,
        default_explorer_turns,
        default_synth_turns,
        &sandbox,
        &persistence,
        session_capacity,
    )
    .await;
    match outcome {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(args.json, kind, message, code),
    }
}

/// A resolution-stage rejection carrying the exit code it maps to. There is
/// deliberately **no** blanket `From<McpError>`: a `McpError` alone can't tell a
/// usage mistake from a boundary block (both are `invalid_params` on the wire), so
/// each resolution call site tags its failure with [`usage`](Self::usage) or
/// [`setup`](Self::setup) explicitly — that's what keeps the exit-code contract at
/// the top of this module honest.
#[derive(Debug)]
struct SetupError {
    kind: &'static str,
    message: String,
    code: i32,
}

impl SetupError {
    /// A **usage** rejection (exit 2, like clap's own argument errors): the caller
    /// named something invalid — a cast that doesn't exist or is wrong for the tool
    /// (a batch/direct cast on interactive `consult`), bad model-override args, or an
    /// image attached to a vision-blind cast. "Fix your command."
    fn usage(e: McpError) -> Self {
        SetupError {
            kind: "usage",
            message: e.message.to_string(),
            code: EXIT_USAGE,
        }
    }

    /// A **setup/containment** rejection (exit 3): a valid-looking request the
    /// environment or boundary blocked — a path or attachment outside the allowed
    /// set, a missing or unbuildable provider key, a house-rules/orientation read
    /// failure. Not the caller's argument mistake; the surroundings.
    fn setup(e: McpError) -> Self {
        SetupError {
            kind: "setup",
            message: e.message.to_string(),
            code: EXIT_SETUP,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_and_run(
    common: &CommonArgs,
    args: &ConsultArgs,
    resolver: &Resolver,
    inline_budget: usize,
    call_deadline: std::time::Duration,
    default_explorer_turns: usize,
    default_synth_turns: usize,
    sandbox: &crate::sandbox::SandboxConfig,
    persistence: &crate::config::PersistenceConfig,
    session_capacity: std::num::NonZeroUsize,
) -> Result<i32, SetupError> {
    // Each call tags its failure explicitly (see `SetupError`): a path/attachment
    // outside the boundary or an unbuildable key is `setup` (exit 3); a wrong-for-the-tool
    // cast, bad override args, or an image on a blind cast is `usage` (exit 2).
    let root = resolver
        .resolve_root(args.path.clone())
        .map_err(SetupError::setup)?;
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    resolver
        .reject_offline_cast(&cast, "consult")
        .map_err(SetupError::usage)?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Explorer,
            args.explorer_model.as_deref(),
            args.explorer_backend.as_deref(),
            "explorer_model",
            "explorer_backend",
        )
        .map_err(SetupError::usage)?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Synth,
            args.synth_model.as_deref(),
            args.synth_backend.as_deref(),
            "synth_model",
            "synth_backend",
        )
        .map_err(SetupError::usage)?;
    let explorer = resolver
        .arm(&cast, ModelRole::Explorer)
        .map_err(SetupError::setup)?;
    let synth = resolver
        .arm(&cast, ModelRole::Synth)
        .map_err(SetupError::setup)?;

    let attachments =
        Resolver::resolve_consult_attachments(&root, &args.attach, inline_budget, sandbox)
            .await
            .map_err(SetupError::setup)?;
    Resolver::gate_consult_image_attachments(
        &attachments,
        synth.caps.vision,
        &synth.model,
        &cast.name,
    )
    .map_err(SetupError::usage)?;

    // Only stand up a session store when a `--session` was named; a stateless consult
    // never opens the db. A failed open is a loud setup error naming the escape hatch.
    let sessions = if args.session.is_some() {
        Some(open_sessions(persistence, session_capacity, resolver).await?)
    } else {
        None
    };
    let session = match (&sessions, &args.session) {
        (Some(s), Some(id)) => Some((s, id.as_str())),
        _ => None,
    };

    let cfg = ConsultConfig {
        explore: ExploreConfig {
            phase: PhaseContext {
                progress: Arc::new(TerminalSink),
                house_rules: resolver.house_rules(&root).map_err(SetupError::setup)?,
                prompts: resolver.resolved_prompts(&cast),
                orientation: resolver
                    .orientation(&root)
                    .await
                    .map_err(SetupError::setup)?,
                call_deadline,
            },
            explorer_max_turns: args.explorer_max_turns.unwrap_or(default_explorer_turns),
            sandbox: sandbox.clone(),
        },
        synth_max_turns: args.synth_max_turns.unwrap_or(default_synth_turns),
        attachments,
    };

    match consult(
        &args.question,
        args.context.as_deref(),
        root,
        &explorer,
        &synth,
        &cfg,
        session,
    )
    .await
    {
        Ok(out) => {
            emit_answer(args, &out, &cast.name, &explorer.model, &synth.model);
            Ok(EXIT_OK)
        }
        // A provider/model-loop failure is its own exit code — the consultation ran and
        // failed, distinct from a setup rejection.
        Err(e) => Ok(fail_consultation(args.json, "consult", &cast.name, e)),
    }
}

/// Open the durable session store when persistence is enabled, else an in-memory one.
/// The durable store is fed the resolver's allowed set so its containment guard
/// refuses a state db inside any project tree — the same wiring `main` uses.
async fn open_sessions(
    persistence: &crate::config::PersistenceConfig,
    session_capacity: std::num::NonZeroUsize,
    resolver: &Resolver,
) -> Result<Sessions, SetupError> {
    if !persistence.enabled {
        return Ok(Sessions::Memory(MemSessionStore::new(session_capacity)));
    }
    let path = persistence.path.clone().ok_or_else(|| SetupError {
        kind: "config",
        message: "persistence is enabled but no state-db path resolved".to_string(),
        code: EXIT_USAGE,
    })?;
    let allowed = resolver.allowed_set();
    let allowed_refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
    let store = crate::store::SessionStore::open(&path, session_capacity, &allowed_refs)
        .await
        .map_err(|e| SetupError {
            kind: "persistence",
            message: format!(
                "failed to open the persistence state db at {}: {e:#}. \
                 Fix the path/permissions, or point elsewhere with --state-db. \
                 Or pass --no-persistence to run this `--session` in memory for this \
                 invocation only — the thread works now but is lost when the process exits \
                 (it won't survive or be shared with the MCP server).",
                path.display()
            ),
            code: EXIT_SETUP,
        })?;
    Ok(Sessions::Persistent(store))
}

/// Print a successful consult answer: the JSON envelope on stdout under `--json`, else
/// the answer with the same provenance footer the MCP tool appends. Progress and logs
/// already went to stderr, so stdout carries only the answer — clean for a pipe.
fn emit_answer(
    args: &ConsultArgs,
    out: &ConsultOutput,
    cast: &str,
    explorer_model: &str,
    synth_model: &str,
) {
    if args.json {
        println!(
            "{}",
            consult_envelope(out, cast, explorer_model, synth_model, args.include_report)
        );
        return;
    }
    let answer = with_provenance(
        out.answer.clone(),
        cast,
        &[("explorer", explorer_model), ("synth", synth_model)],
        &out.usage,
    );
    println!("{answer}");
    // Non-fatal warnings (a failed session record) go to STDERR, never stdout — stdout
    // stays the model's answer, clean for a pipe. (`--json` carries them structured
    // instead; see `consult_envelope`.)
    for w in &out.warnings {
        eprintln!("kaibo: {w}");
    }
    // The report is opt-in extra; keep it off stdout's answer line — send it to stderr
    // so a pipe still captures just the answer.
    if args.include_report && !out.report.is_empty() {
        eprintln!("\n--- explorer report ---\n{}", out.report);
    }
}

/// The reported token usage as a stable JSON object — one shape across every `--json`
/// envelope (consult/oneshot/explore). Pure and testable.
fn usage_json(usage: &Usage) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
    })
}

/// The `--json` success envelope: the raw answer (no footer), provenance, and usage —
/// a stable shape for a script. Pure, so it's unit-testable without a model.
fn consult_envelope(
    out: &ConsultOutput,
    cast: &str,
    explorer_model: &str,
    synth_model: &str,
    include_report: bool,
) -> serde_json::Value {
    let mut env = serde_json::json!({
        // The raw model answer — no footer, no injected notices. A machine consumer
        // (`jq -r .answer`) gets the model's words uncorrupted; kaibo's own non-fatal
        // notices ride the separate `warnings` array below.
        "answer": out.answer,
        "cast": cast,
        "models": { "explorer": explorer_model, "synth": synth_model },
        "usage": usage_json(&out.usage),
        // Non-fatal notices about this turn (e.g. a failed session record). Always
        // present (empty when the turn was clean) so a consumer can rely on the key.
        "warnings": out.warnings,
    });
    if include_report {
        env["report"] = serde_json::Value::String(out.report.clone());
    }
    env
}

/// The `--json` error envelope: `{ "error": …, "kind": … }`. Pure and testable.
fn error_envelope(kind: &str, message: &str) -> serde_json::Value {
    serde_json::json!({ "error": message, "kind": kind })
}

/// Render a runtime consultation failure (a provider/model-loop error, distinct from a
/// setup rejection) and return [`EXIT_CONSULT_FAILURE`]. Reuses the server's classified
/// failure text so the CLI and MCP tool frame a failure identically; `--json` wraps it in
/// the error envelope. Shared by consult/oneshot/explore/batch-submit.
fn fail_consultation(json: bool, tool: &str, cast: &str, err: anyhow::Error) -> i32 {
    let text = consultation_failure_text(tool, cast, err);
    if json {
        println!("{}", error_envelope("consultation_failure", &text));
    } else {
        eprintln!("kaibo: {text}");
    }
    EXIT_CONSULT_FAILURE
}

/// Append context piped on stdin to the prompt (the `oneshot "…" < file` idiom). Only
/// reads stdin when it's NOT a terminal (piped/redirected), so an interactive
/// `kaibo oneshot "…"` never blocks waiting for input. Whitespace-only stdin is ignored.
///
/// Non-empty, non-UTF-8 stdin is a **loud usage error** (exit 2), never a silent drop:
/// `kaibo oneshot "…" < image.png` must not run the model blind about data it never
/// received. oneshot takes *text* on stdin; a file (incl. an image, on a vision cast)
/// belongs on `--attach`.
fn prompt_with_stdin(prompt: &str) -> Result<String, SetupError> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return Ok(prompt.to_string());
    }
    let mut bytes = Vec::new();
    // A read error (e.g. stdin already closed) is treated as "no piped context" — the
    // bare prompt still runs. Only *present but non-text* input is the loud error.
    if std::io::stdin().read_to_end(&mut bytes).is_err() {
        return Ok(prompt.to_string());
    }
    fold_stdin_context(prompt, &bytes)
}

/// Pure core of [`prompt_with_stdin`]: fold already-read stdin `bytes` into the prompt.
/// Empty / whitespace-only → the bare prompt; UTF-8 text → appended after a blank line;
/// non-empty non-UTF-8 → a loud usage error. Split out so the fail-loud contract is
/// testable without touching process stdin.
fn fold_stdin_context(prompt: &str, bytes: &[u8]) -> Result<String, SetupError> {
    if bytes.is_empty() {
        return Ok(prompt.to_string());
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if text.trim().is_empty() => Ok(prompt.to_string()),
        Ok(text) => Ok(format!("{prompt}\n\n{}", text.trim_end())),
        Err(_) => Err(SetupError {
            kind: "usage",
            message: format!(
                "oneshot reads TEXT context on stdin, but the piped input isn't valid UTF-8 \
                 ({} bytes) — kaibo won't send the model a prompt about data it never got. \
                 Pipe text, or pass the file with --attach (an image needs a vision-capable cast).",
                bytes.len()
            ),
            code: EXIT_USAGE,
        }),
    }
}

/// Run `kaibo config`: print the resolved configuration the way the `kaibo://config`
/// resource renders it — reusing the exact renderer so the two never drift.
pub fn run_config(common: CommonArgs) -> i32 {
    init_cli_logging();
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kaibo: config error: {e:#}");
            return EXIT_USAGE;
        }
    };
    let persistence_enabled = config.persistence.enabled;
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kaibo: {e:#}");
            return EXIT_SETUP;
        }
    };
    // `active` reflects whether a real invocation would hold the store open — for a
    // one-shot `config` print we don't open it, but persistence being enabled is what
    // a consult here would activate, so report that.
    let body = render_config_resource(
        &resolver.config,
        resolver.allowed_trees(),
        resolver.default_root_ref(),
        resolver.default_root_inferred(),
        resolver.followed_worktrees(),
        persistence_enabled,
    );
    println!("{body}");
    EXIT_OK
}

// ---------------------------------------------------------------------------
// oneshot
// ---------------------------------------------------------------------------

/// Run `kaibo oneshot` — a toolless second opinion. Same stdout/stderr contract and
/// exit taxonomy (usage 2 / setup 3 / failure 4) as consult.
pub async fn run_oneshot(common: CommonArgs, args: OneshotArgs) -> i32 {
    init_cli_logging();
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            return fail_preflight(
                args.json,
                "config",
                format!("config error: {e:#}"),
                EXIT_USAGE,
            )
        }
    };
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => return fail_preflight(args.json, "setup", format!("{e:#}"), EXIT_SETUP),
    };
    match oneshot_inner(&common, &args, &resolver).await {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(args.json, kind, message, code),
    }
}

async fn oneshot_inner(
    common: &CommonArgs,
    args: &OneshotArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    resolver
        .reject_offline_cast(&cast, "oneshot")
        .map_err(SetupError::usage)?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Synth,
            args.model.as_deref(),
            args.backend.as_deref(),
            "model",
            "backend",
        )
        .map_err(SetupError::usage)?;
    let arm = resolver
        .arm(&cast, ModelRole::Synth)
        .map_err(SetupError::setup)?;
    // Attachments read + containment-checked server-side (bytes never transit your
    // context); an image needs a vision-capable cast.
    let attachments = resolver
        .resolve_attachments(&args.attach)
        .await
        .map_err(SetupError::setup)?;
    resolver
        .gate_image_attachments(arm.caps.vision, &attachments, &arm.model, &cast.name)
        .map_err(SetupError::usage)?;
    // Fold any context piped on stdin into the prompt (the `< file` idiom); non-text
    // piped input is a loud usage error rather than a silent drop.
    let prompt = prompt_with_stdin(&args.prompt)?;
    let cfg = PhaseContext {
        progress: Arc::new(TerminalSink),
        // oneshot reads no project: no house rules, no repo map, no shell.
        house_rules: None,
        prompts: resolver.resolved_prompts(&cast),
        orientation: None,
        call_deadline: resolver.config.defaults.call_deadline,
    };
    match run_oneshot_engine(&prompt, &attachments, &arm, &cfg).await {
        Ok((answer, usage)) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "answer": answer,
                        "cast": cast.name,
                        "model": arm.model,
                        "usage": usage_json(&usage),
                    })
                );
            } else {
                println!(
                    "{}",
                    with_provenance(answer, &cast.name, &[("model", &arm.model)], &usage)
                );
            }
            Ok(EXIT_OK)
        }
        Err(e) => Ok(fail_consultation(args.json, "oneshot", &cast.name, e)),
    }
}

// ---------------------------------------------------------------------------
// explore
// ---------------------------------------------------------------------------

/// Run `kaibo explore` — a cited survey report. Same conventions as consult; the
/// payload is the report (`report` field under `--json`).
pub async fn run_explore(common: CommonArgs, args: ExploreArgs) -> i32 {
    init_cli_logging();
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            return fail_preflight(
                args.json,
                "config",
                format!("config error: {e:#}"),
                EXIT_USAGE,
            )
        }
    };
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => return fail_preflight(args.json, "setup", format!("{e:#}"), EXIT_SETUP),
    };
    match explore_inner(&common, &args, &resolver).await {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(args.json, kind, message, code),
    }
}

async fn explore_inner(
    common: &CommonArgs,
    args: &ExploreArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    let root = resolver
        .resolve_root(args.path.clone())
        .map_err(SetupError::setup)?;
    // NO reject_offline_cast: explore runs the *explorer* arm, so a deliberate/direct
    // cast's explorer is valid; it needs only an explorer slot (resolved next).
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Explorer,
            args.explorer_model.as_deref(),
            args.explorer_backend.as_deref(),
            "explorer_model",
            "explorer_backend",
        )
        .map_err(SetupError::usage)?;
    let explorer = resolver
        .arm(&cast, ModelRole::Explorer)
        .map_err(SetupError::setup)?;
    let attachments = resolver
        .resolve_sweep_attachments(&root, &args.attach, "explore")
        .await
        .map_err(SetupError::setup)?;
    let cfg = ExploreConfig {
        phase: PhaseContext {
            progress: Arc::new(TerminalSink),
            house_rules: resolver.house_rules(&root).map_err(SetupError::setup)?,
            prompts: resolver.resolved_prompts(&cast),
            orientation: resolver
                .orientation(&root)
                .await
                .map_err(SetupError::setup)?,
            call_deadline: resolver.config.defaults.call_deadline,
        },
        explorer_max_turns: args
            .explorer_max_turns
            .unwrap_or(resolver.config.defaults.explorer_max_turns),
        sandbox: resolver.config.sandbox.clone(),
    };
    match explore_with(&args.question, root, &explorer, &cfg, &attachments).await {
        Ok((report, usage)) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "report": report,
                        "cast": cast.name,
                        "model": explorer.model,
                        "usage": usage_json(&usage),
                    })
                );
            } else {
                println!(
                    "{}",
                    with_provenance(report, &cast.name, &[("explorer", &explorer.model)], &usage)
                );
            }
            Ok(EXIT_OK)
        }
        Err(e) => Ok(fail_consultation(args.json, "explore", &cast.name, e)),
    }
}

// ---------------------------------------------------------------------------
// kaish
// ---------------------------------------------------------------------------

/// Run `kaibo kaish -c 'SCRIPT'` — one non-interactive execution through the read-only
/// sandbox. stdout carries the script's stdout, stderr its stderr, and the process exits
/// with kaish's own exit code (0 ok, 126 blocked, 124 timed out). A missing `-c` is a
/// usage error (exit 2); a bad `--path` is a setup rejection (exit 3).
pub async fn run_kaish(common: CommonArgs, args: KaishArgs) -> i32 {
    init_cli_logging();
    let Some(script) = args.command.clone() else {
        let msg = "kaish needs a script — pass it with `-c 'SCRIPT'` (kaibo has no \
                   interactive shell). e.g. kaibo kaish -c 'grep -rn TODO src/'"
            .to_string();
        if args.json {
            println!("{}", error_envelope("usage", &msg));
        } else {
            eprintln!("kaibo: {msg}");
        }
        return EXIT_USAGE;
    };
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("config error: {e:#}");
            if args.json {
                println!("{}", error_envelope("config", &msg));
            } else {
                eprintln!("kaibo: {msg}");
            }
            return EXIT_USAGE;
        }
    };
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => {
            if args.json {
                println!("{}", error_envelope("setup", &format!("{e:#}")));
            } else {
                eprintln!("kaibo: {e:#}");
            }
            return EXIT_SETUP;
        }
    };
    let root = match resolver.resolve_root(args.path.clone()) {
        Ok(r) => r,
        Err(e) => {
            let msg = e.message.to_string();
            if args.json {
                println!("{}", error_envelope("setup", &msg));
            } else {
                eprintln!("kaibo: {msg}");
            }
            return EXIT_SETUP;
        }
    };
    let worker = match KaishWorker::spawn_with(&root, resolver.config.sandbox.clone()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("kaibo: could not start kaish: {e:#}");
            return EXIT_SETUP;
        }
    };
    match worker.run(script).await {
        Ok(out) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "stdout": out.stdout,
                        "stderr": out.stderr,
                        "exit_code": out.code,
                    })
                );
            } else {
                // Scriptable: the script's own streams, unframed, on our streams.
                if !out.stdout.is_empty() {
                    print!("{}", out.stdout);
                    if !out.stdout.ends_with('\n') {
                        println!();
                    }
                }
                if !out.stderr.is_empty() {
                    eprint!("{}", out.stderr);
                    if !out.stderr.ends_with('\n') {
                        eprintln!();
                    }
                }
            }
            // Exit with kaish's own code so a script can branch on it.
            out.code as i32
        }
        // A worker.run() error is a RUNTIME infra failure (kernel crash/panic, worker
        // channel closed) — the shell ran (or tried to) and failed, not a pre-flight
        // rejection — so it's exit 4, not a setup code. (An honest script/blocked/timeout
        // outcome came back Ok(out) above with kaish's own code.)
        Err(e) => {
            eprintln!("kaibo: kaish execution failed: {e:#}");
            EXIT_CONSULT_FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// batch
// ---------------------------------------------------------------------------

/// Run `kaibo batch submit|get|list`.
pub async fn run_batch(common: CommonArgs, args: BatchArgs) -> i32 {
    init_cli_logging();
    let json = match &args.cmd {
        BatchCmd::Submit(a) => a.json,
        BatchCmd::Work(a) => a.json,
        BatchCmd::Get(a) => a.json,
        BatchCmd::List(a) => a.json,
        BatchCmd::Cancel(a) => a.json,
    };
    let config = match load_config(&common) {
        Ok(c) => c,
        Err(e) => {
            return fail_preflight(json, "config", format!("config error: {e:#}"), EXIT_USAGE)
        }
    };
    let resolver = match Resolver::from_config(Arc::new(config)) {
        Ok(r) => r,
        Err(e) => return fail_preflight(json, "setup", format!("{e:#}"), EXIT_SETUP),
    };
    let outcome = match args.cmd {
        BatchCmd::Submit(a) => batch_submit_inner(&common, &a, &resolver).await,
        BatchCmd::Work(a) => batch_work_inner(&a, &resolver).await,
        BatchCmd::Get(a) => batch_get_inner(&a, &resolver).await,
        BatchCmd::List(a) => batch_list_inner(&a, &resolver).await,
        BatchCmd::Cancel(a) => batch_cancel_inner(&a, &resolver).await,
    };
    match outcome {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(json, kind, message, code),
    }
}

/// Open the durable store for batch-handle persistence/recovery — best-effort: `None`
/// when persistence is off, and a warn (never fatal) if the open fails, since a batch is
/// live at the provider regardless and `batch list`'s provider query still recovers it.
async fn open_batch_store(resolver: &Resolver) -> Option<crate::store::SessionStore> {
    let persistence = &resolver.config.persistence;
    if !persistence.enabled {
        return None;
    }
    let path = persistence.path.clone()?;
    let cap = resolver.config.defaults.session_capacity;
    let allowed = resolver.allowed_set();
    let refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
    match crate::store::SessionStore::open(&path, cap, &refs).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "could not open the state db for batch handles — continuing without persistence");
            None
        }
    }
}

/// The backend names `batch list` should query — mirrors the MCP handler's rule: an
/// explicit `--backend` scopes to that one (refused if its kind has no batch lane); else
/// every configured batch-capable backend, sorted-by-map-order.
fn batch_backends(resolver: &Resolver, backend: Option<&str>) -> Result<Vec<String>, SetupError> {
    use crate::batch::{batch_supported, supported_kinds_list};
    if let Some(name) = backend {
        let b = resolver
            .config
            .resolve_backend(name)
            .map_err(|e| SetupError {
                kind: "usage",
                message: e.to_string(),
                code: EXIT_USAGE,
            })?;
        if !batch_supported(b.kind) {
            return Err(SetupError {
                kind: "usage",
                message: format!(
                    "backend {:?} ({:?}) has no batch lane (batch-capable: {}). Omit --backend \
                     to list every batch-capable backend.",
                    b.name,
                    b.kind,
                    supported_kinds_list()
                ),
                code: EXIT_USAGE,
            });
        }
        return Ok(vec![b.name.clone()]);
    }
    let names: Vec<String> = resolver
        .config
        .backends
        .values()
        .filter(|b| batch_supported(b.kind))
        .map(|b| b.name.clone())
        .collect();
    if names.is_empty() {
        return Err(SetupError {
            kind: "setup",
            message: "no batch-capable backend is configured".to_string(),
            code: EXIT_SETUP,
        });
    }
    Ok(names)
}

async fn batch_submit_inner(
    common: &CommonArgs,
    args: &BatchSubmitArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    // The local lane is a different animal — no provider, no batch-cast requirement, the
    // state db as the queue — so branch before touching any provider machinery.
    if args.local {
        return batch_local_submit_inner(common, args, resolver).await;
    }
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    resolver
        .require_batch_cast(&cast)
        .map_err(SetupError::usage)?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Synth,
            args.model.as_deref(),
            args.backend.as_deref(),
            "model",
            "backend",
        )
        .map_err(SetupError::usage)?;
    // Resolve the synth slot + backend + caps (key-free — no network yet).
    let slot = cast
        .require_slot(ModelRole::Synth)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    let backend = resolver
        .config
        .resolve_backend(&slot.backend)
        .map_err(|e| SetupError {
            kind: "setup",
            message: e.to_string(),
            code: EXIT_SETUP,
        })?;
    let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);
    let backend_name = backend.name.clone();
    let model = slot.id.clone();
    // Attachments (shared context) + vision gate before the network — a bad path or a
    // vision misconfig is a clean refusal, not a half-submitted batch.
    let attachments = resolver
        .resolve_attachments(&args.attach)
        .await
        .map_err(SetupError::setup)?;
    resolver
        .gate_image_attachments(caps.vision, &attachments, &model, &cast.name)
        .map_err(SetupError::usage)?;
    let provider =
        crate::batch::submitter(backend, slot, &resolver.config.defaults).map_err(|e| {
            SetupError {
                kind: "setup",
                message: format!("{e:#}"),
                code: EXIT_SETUP,
            }
        })?;
    let items: Vec<BatchItem> = args
        .prompts
        .iter()
        .enumerate()
        .map(|(i, p)| BatchItem {
            custom_id: i.to_string(),
            prompt: p.clone(),
        })
        .collect();
    let system = batch_system_prompt(resolver.resolved_prompts(&cast).batch.as_deref());
    let provider_id = match provider.submit(&system, &attachments, &items).await {
        Ok(id) => id,
        // A provider submit failure ran and failed — exit 4, like a consultation failure.
        Err(e) => return Ok(fail_consultation(args.json, "batch submit", &cast.name, e)),
    };
    let handle = format!("{backend_name}/{provider_id}");
    // Persist the handle so a restart can re-list it (best-effort; the batch is already
    // live at the provider).
    if let Some(store) = open_batch_store(resolver).await {
        if let Err(e) = store
            .put_batch(&backend_name, &provider_id, Some(&model))
            .await
        {
            tracing::warn!(handle = %handle, error = %e, "could not persist batch handle");
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "handle": handle,
                "cast": cast.name,
                "model": model,
                "count": items.len(),
            })
        );
    } else {
        // Payload = the durable handle on stdout; the human note to stderr.
        println!("{handle}");
        eprintln!(
            "kaibo: submitted {} prompt(s) on cast `{}` (model `{}`) — `kaibo batch get {}` for results",
            items.len(),
            cast.name,
            model,
            handle
        );
    }
    Ok(EXIT_OK)
}

async fn batch_get_inner(args: &BatchGetArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    // A `local/<id>` handle collects from the state db, not a provider (checked before the
    // `backend/provider-id` split, since a local handle also carries a `/`).
    if is_local_handle(&args.handle) {
        return batch_get_local_inner(args, resolver).await;
    }
    let (backend_name, provider_id) = args
        .handle
        .split_once('/')
        .filter(|(b, id)| !b.is_empty() && !id.is_empty())
        .ok_or_else(|| SetupError {
            kind: "usage",
            message: format!(
                "batch handle {:?} must be \"backend/provider-id\" — pass the handle \
                 `kaibo batch submit` printed",
                args.handle
            ),
            code: EXIT_USAGE,
        })?;
    let backend = resolver
        .config
        .resolve_backend(backend_name)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    let provider = crate::batch::poller(backend).map_err(|e| SetupError {
        kind: "setup",
        message: format!("{e:#}"),
        code: EXIT_SETUP,
    })?;
    match provider.poll(provider_id).await {
        Ok(poll) => {
            if args.json {
                println!("{}", batch_poll_envelope(&poll));
            } else {
                println!(
                    "{}",
                    crate::batch::render_poll(&poll, &format!("{backend_name} · {provider_id}"))
                );
            }
            Ok(EXIT_OK)
        }
        Err(e) => Ok(fail_consultation(args.json, "batch get", backend_name, e)),
    }
}

/// A `BatchPoll` as a stable JSON object for `batch get --json`. Pure and testable.
fn batch_poll_envelope(poll: &BatchPoll) -> serde_json::Value {
    match poll {
        BatchPoll::Pending { completed, total } => {
            serde_json::json!({ "status": "pending", "completed": completed, "total": total })
        }
        BatchPoll::Cancelling => serde_json::json!({ "status": "cancelling" }),
        BatchPoll::Done(answers) => serde_json::json!({
            "status": "done",
            "answers": answers.iter().map(|a| match &a.text {
                Ok(t) => serde_json::json!({ "custom_id": a.custom_id, "ok": true, "text": t }),
                Err(reason) => serde_json::json!({ "custom_id": a.custom_id, "ok": false, "error": reason }),
            }).collect::<Vec<_>>(),
        }),
        BatchPoll::Failed { state, message } => {
            serde_json::json!({ "status": "failed", "state": state, "message": message })
        }
    }
}

async fn batch_list_inner(args: &BatchListArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let backends = batch_backends(resolver, args.backend.as_deref())?;
    let mut entries: Vec<(String, crate::batch::BatchListItem)> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    let mut truncated: Vec<String> = Vec::new();
    let mut shown: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &backends {
        let backend = match resolver.config.resolve_backend(name) {
            Ok(b) => b,
            Err(e) => {
                errors.push((name.clone(), e.to_string()));
                continue;
            }
        };
        let provider = match crate::batch::poller(backend) {
            Ok(p) => p,
            Err(e) => {
                errors.push((name.clone(), format!("{e:#}")));
                continue;
            }
        };
        match provider.list().await {
            Ok((items, has_more)) => {
                if has_more {
                    truncated.push(name.clone());
                }
                for it in items {
                    let handle = format!("{name}/{}", it.provider_id);
                    shown.insert(handle.clone());
                    entries.push((handle, it));
                }
            }
            Err(e) => errors.push((name.clone(), format!("{e:#}"))),
        }
    }
    // Trim to the last 24h unless --all (a provider keeps months of finished batches).
    let hidden = if args.all {
        0
    } else {
        let now = now_epoch_secs();
        let before = entries.len();
        entries.retain(|(_, it)| batch_within_window(it, now, BATCH_RECENCY_WINDOW_SECS));
        before - entries.len()
    };
    // Recovered handles + local jobs from the store, deduped against the live listing.
    let mut recovered: Vec<(String, Option<String>)> = Vec::new();
    let mut local_jobs: Vec<crate::store::LocalJobSummary> = Vec::new();
    if let Some(store) = open_batch_store(resolver).await {
        match store.list_batches().await {
            Ok(handles) => {
                for h in handles {
                    let handle = format!("{}/{}", h.backend, h.provider_id);
                    if !shown.contains(&handle) {
                        recovered.push((handle, h.label));
                    }
                }
            }
            Err(e) => errors.push((
                "(store)".to_string(),
                format!("recovered handles unavailable: {e}"),
            )),
        }
        match store.list_local().await {
            Ok(js) => local_jobs = js,
            Err(e) => errors.push((
                "(store)".to_string(),
                format!("local jobs unavailable: {e}"),
            )),
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "entries": entries.iter().map(|(h, it)| serde_json::json!({
                    "handle": h,
                    "status": it.status,
                    "completed": it.completed,
                    "total": it.total,
                    "created_at": it.created_at,
                })).collect::<Vec<_>>(),
                "recovered": recovered.iter().map(|(h, l)| serde_json::json!({ "handle": h, "label": l })).collect::<Vec<_>>(),
                "local": local_jobs.iter().map(|j| serde_json::json!({
                    "handle": format!("local/{}", j.id),
                    "status": j.status.as_str(),
                    "cast": j.cast,
                    "total": j.total,
                    "done": j.done,
                    "failed": j.failed,
                    "created_at": j.created_at,
                })).collect::<Vec<_>>(),
                "errors": errors.iter().map(|(b, e)| serde_json::json!({ "backend": b, "error": e })).collect::<Vec<_>>(),
                "hidden": hidden,
                "truncated": truncated,
            })
        );
    } else {
        println!(
            "{}",
            crate::batch::render_list(&entries, &errors, &truncated)
        );
        if !local_jobs.is_empty() {
            println!("\n{}", render_local_list(&local_jobs));
        }
        if hidden > 0 {
            println!(
                "\n({hidden} batch(es) older than 24h hidden — `kaibo batch list --all` for the \
                 full history.)"
            );
        }
        if !recovered.is_empty() {
            let lines: Vec<String> = recovered
                .iter()
                .map(|(h, l)| match l {
                    Some(l) => format!("- `{h}` — {l}"),
                    None => format!("- `{h}`"),
                })
                .collect();
            println!(
                "\nRecovered batch handles (kaibo-submitted, from the store — `kaibo batch get` \
                 one for live status):\n{}",
                lines.join("\n")
            );
        }
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------
// local batch
// ---------------------------------------------------------------------------

/// Open the durable store, requiring persistence — local batch lives in the state db (it's
/// the queue AND the mailbox), so with persistence off there is nowhere for a local job to
/// exist. A clear setup refusal, not a silent no-op.
async fn open_required_store(
    resolver: &Resolver,
) -> Result<crate::store::SessionStore, SetupError> {
    let persistence = &resolver.config.persistence;
    if !persistence.enabled {
        return Err(SetupError {
            kind: "setup",
            message: "local batch needs persistence (the state db is its queue and mailbox), \
                      but it is disabled (--no-persistence / KAIBO_NO_PERSISTENCE). Enable \
                      persistence to submit, run, and collect local batches."
                .to_string(),
            code: EXIT_SETUP,
        });
    }
    let path = persistence.path.clone().ok_or_else(|| SetupError {
        kind: "config",
        message: "persistence is enabled but no state-db path resolved".to_string(),
        code: EXIT_USAGE,
    })?;
    let cap = resolver.config.defaults.session_capacity;
    let allowed = resolver.allowed_set();
    let refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
    crate::store::SessionStore::open(&path, cap, &refs)
        .await
        .map_err(|e| SetupError {
            kind: "setup",
            message: format!(
                "failed to open the persistence state db at {}: {e:#}",
                path.display()
            ),
            code: EXIT_SETUP,
        })
}

/// `kaibo batch submit --local` — enqueue a local batch job to the state db and print the
/// durable `local/<id>` handle. Any cast works (that's the point — local compute / a
/// `direct`-lane big local model / anything); attachment CONTENT is captured now (behind the
/// store guard) so the worker feeds the model the bytes as they were at submit, and image
/// attachments are vision-gated here just like a provider batch.
async fn batch_local_submit_inner(
    common: &CommonArgs,
    args: &BatchSubmitArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    let store = open_required_store(resolver).await?;
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    // Deliberately NO require_batch_cast / reject_offline_cast: local batch runs any cast,
    // interactive or offline — the worker drives each item through the toolless oneshot path.
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Synth,
            args.model.as_deref(),
            args.backend.as_deref(),
            "model",
            "backend",
        )
        .map_err(SetupError::usage)?;
    // Resolve the synth slot + caps (key-free) for the vision gate.
    let slot = cast
        .require_slot(ModelRole::Synth)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    let backend = resolver
        .config
        .resolve_backend(&slot.backend)
        .map_err(|e| SetupError {
            kind: "setup",
            message: e.to_string(),
            code: EXIT_SETUP,
        })?;
    let caps = ModelCaps::resolve(backend.kind, &slot.id, slot.vision);
    let model = slot.id.clone();
    // Capture attachment CONTENT now (kaibo reads them; bytes never transit the caller's
    // context) + vision gate before enqueue.
    let attachments = resolver
        .resolve_attachments(&args.attach)
        .await
        .map_err(SetupError::setup)?;
    resolver
        .gate_image_attachments(caps.vision, &attachments, &model, &cast.name)
        .map_err(SetupError::usage)?;

    let id = store
        .enqueue_local(
            &cast.name,
            args.model.as_deref(),
            args.backend.as_deref(),
            &attachments,
            &args.prompts,
        )
        .await
        .map_err(|e| SetupError {
            kind: "setup",
            message: format!("could not enqueue the local batch: {e:#}"),
            code: EXIT_SETUP,
        })?;
    let handle = format!("local/{id}");
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "handle": handle,
                "cast": cast.name,
                "model": model,
                "count": args.prompts.len(),
                "local": true,
            })
        );
    } else {
        println!("{handle}");
        eprintln!(
            "kaibo: enqueued {} prompt(s) as `{}` on cast `{}` — run `kaibo batch work` to \
             drain it (background with `&`/systemd-run/cron), then `kaibo batch get {}`.",
            args.prompts.len(),
            handle,
            cast.name,
            handle
        );
    }
    Ok(EXIT_OK)
}

/// `kaibo batch work` — the foreground worker. Claims pending jobs one at a time (the claim
/// is a `BEGIN IMMEDIATE` status flip, so two workers on one db never double-run a job),
/// runs each item through the oneshot engine on the job's cast (resolved against THIS
/// process's config), and writes per-item results as they complete — a crash loses at most
/// the in-flight item. Drains to empty and exits 0; `--watch SECS` keeps polling instead.
async fn batch_work_inner(args: &BatchWorkArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let store = open_required_store(resolver).await?;
    let mut drained = 0usize;
    loop {
        loop {
            match store.claim_next_local().await {
                Ok(Some(id)) => {
                    work_one_local_job(&store, resolver, id).await;
                    drained += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    // A claim failure is a real infra problem (the queue db is unreadable) —
                    // stop loudly rather than spin.
                    return Err(SetupError {
                        kind: "setup",
                        message: format!("could not claim the next local job: {e:#}"),
                        code: EXIT_SETUP,
                    });
                }
            }
        }
        match args.watch {
            Some(secs) if secs > 0 => {
                eprintln!("kaibo: queue drained ({drained} so far) — watching, polling every {secs}s (Ctrl-C to stop).");
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
            _ => break,
        }
    }
    if args.json {
        println!("{}", serde_json::json!({ "drained": drained }));
    } else {
        println!("kaibo: worker drained {drained} local batch job(s).");
    }
    Ok(EXIT_OK)
}

/// Run one claimed local job to completion. Resolves the job's cast against the worker's own
/// config; a setup failure (unknown cast, unbuildable key) marks every unfinished item with
/// that reason and the job `failed`. Otherwise runs each not-yet-done item through the
/// toolless oneshot engine, recording each result as it lands, checking for a cancel between
/// items, then marks the job `done` (guarded so a concurrent cancel wins). Never returns an
/// error — a per-job failure is captured in the job, so it can't sink the whole worker.
async fn work_one_local_job(store: &crate::store::SessionStore, resolver: &Resolver, id: i64) {
    let job = match store.get_local(id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            eprintln!("kaibo: local/{id} vanished after claim — skipping.");
            return;
        }
        Err(e) => {
            eprintln!("kaibo: could not load local/{id}: {e:#} — skipping.");
            return;
        }
    };
    eprintln!(
        "kaibo: running local/{id} — cast `{}`, {} item(s).",
        job.cast,
        job.items.len()
    );

    // Resolve the cast → synth arm + oneshot context against the worker's own config.
    let (arm, ctx) = match resolve_local_setup(resolver, &job) {
        Ok(pair) => pair,
        Err(reason) => {
            eprintln!("kaibo: local/{id} setup failed: {reason}");
            for item in &job.items {
                if item.result.is_none() {
                    let _ = store
                        .record_local_item(id, item.seq, Err(reason.clone()))
                        .await;
                }
            }
            let _ = store.mark_local_finished(id, LocalJobStatus::Failed).await;
            return;
        }
    };

    for item in &job.items {
        // Resume support: an item already recorded (a worker restarted mid-job) is skipped.
        if item.result.is_some() {
            continue;
        }
        // Honest cancel semantics: the worker checks status BETWEEN items — a running item
        // always finishes, a cancel stops the rest.
        match store.local_status(id).await {
            Ok(Some(LocalJobStatus::Cancelled)) => {
                eprintln!("kaibo: local/{id} cancelled — stopping between items.");
                return;
            }
            Ok(Some(LocalJobStatus::Running)) => {}
            Ok(other) => {
                eprintln!("kaibo: local/{id} is no longer running ({other:?}) — stopping.");
                return;
            }
            Err(e) => eprintln!("kaibo: local/{id} status check failed: {e:#} — continuing."),
        }
        eprintln!("kaibo: local/{id} item {} running…", item.seq);
        let result = match run_oneshot_engine(&item.prompt, &job.attachments, &arm, &ctx).await {
            Ok((answer, _usage)) => Ok(answer),
            Err(e) => Err(format!("{e:#}")),
        };
        if let Err(e) = store.record_local_item(id, item.seq, result).await {
            eprintln!(
                "kaibo: could not record item {} of local/{id}: {e:#}",
                item.seq
            );
        }
    }
    let _ = store.mark_local_finished(id, LocalJobStatus::Done).await;
    eprintln!("kaibo: local/{id} done.");
}

/// Resolve a claimed job's cast into a synth [`Arm`] + oneshot [`PhaseContext`], using the
/// worker's own config. Returns the failure reason as a string (captured onto the job's
/// items) rather than a `SetupError` — the worker keeps going.
fn resolve_local_setup(
    resolver: &Resolver,
    job: &LocalJob,
) -> Result<(crate::consult::Arm, PhaseContext), String> {
    let mut cast = resolver
        .resolve_cast(Some(job.cast.clone()))
        .map_err(|e| e.message.to_string())?;
    resolver
        .apply_model_override(
            &mut cast,
            ModelRole::Synth,
            job.model.as_deref(),
            job.backend.as_deref(),
            "model",
            "backend",
        )
        .map_err(|e| e.message.to_string())?;
    let arm = resolver
        .arm(&cast, ModelRole::Synth)
        .map_err(|e| e.message.to_string())?;
    // oneshot reads no project: no house rules, no orientation, no shell. Quiet progress —
    // the worker prints its own per-item narrative to stderr.
    let ctx = PhaseContext {
        progress: Arc::new(NullSink),
        house_rules: None,
        prompts: resolver.resolved_prompts(&cast),
        orientation: None,
        call_deadline: resolver.config.defaults.call_deadline,
    };
    Ok((arm, ctx))
}

/// `kaibo batch get local/<id>` — render a local job's status/results (prose or `--json`).
async fn batch_get_local_inner(
    args: &BatchGetArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    let store = open_required_store(resolver).await?;
    let id = parse_local_handle(&args.handle).map_err(|e| SetupError {
        kind: "usage",
        message: e.message.to_string(),
        code: EXIT_USAGE,
    })?;
    let job = store
        .get_local(id)
        .await
        .map_err(|e| SetupError {
            kind: "setup",
            message: format!("could not read local job: {e:#}"),
            code: EXIT_SETUP,
        })?
        .ok_or_else(|| SetupError {
            kind: "usage",
            message: format!("no local batch job `{}` — check the handle.", args.handle),
            code: EXIT_USAGE,
        })?;
    if args.json {
        println!("{}", local_job_envelope(&job));
    } else {
        println!("{}", render_local_job(&job));
    }
    Ok(EXIT_OK)
}

/// A local job as a stable JSON object for `batch get local/<id> --json`. Pure and testable.
fn local_job_envelope(job: &LocalJob) -> serde_json::Value {
    serde_json::json!({
        "handle": format!("local/{}", job.id),
        "status": job.status.as_str(),
        "cast": job.cast,
        "items": job.items.iter().map(|i| match &i.result {
            Some(Ok(t)) => serde_json::json!({ "seq": i.seq, "ok": true, "text": t }),
            Some(Err(e)) => serde_json::json!({ "seq": i.seq, "ok": false, "error": e }),
            None => serde_json::json!({ "seq": i.seq, "pending": true }),
        }).collect::<Vec<_>>(),
    })
}

/// `kaibo batch cancel <handle>` — a provider `backend/provider-id` or a `local/<id>` job.
async fn batch_cancel_inner(
    args: &BatchCancelArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    if is_local_handle(&args.handle) {
        let store = open_required_store(resolver).await?;
        let id = parse_local_handle(&args.handle).map_err(|e| SetupError {
            kind: "usage",
            message: e.message.to_string(),
            code: EXIT_USAGE,
        })?;
        let outcome = store.cancel_local(id).await.map_err(|e| SetupError {
            kind: "setup",
            message: format!("could not cancel local job: {e:#}"),
            code: EXIT_SETUP,
        })?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({ "handle": args.handle, "outcome": local_cancel_str(outcome) })
            );
        } else {
            println!("{}", render_local_cancel(&args.handle, outcome));
        }
        return Ok(EXIT_OK);
    }
    // Provider batch cancel.
    let (backend_name, provider_id) = args
        .handle
        .split_once('/')
        .filter(|(b, id)| !b.is_empty() && !id.is_empty())
        .ok_or_else(|| SetupError {
            kind: "usage",
            message: format!(
                "batch handle {:?} must be \"backend/provider-id\" or \"local/<id>\"",
                args.handle
            ),
            code: EXIT_USAGE,
        })?;
    let backend = resolver
        .config
        .resolve_backend(backend_name)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    let provider = crate::batch::poller(backend).map_err(|e| SetupError {
        kind: "setup",
        message: format!("{e:#}"),
        code: EXIT_SETUP,
    })?;
    match provider.cancel(provider_id).await {
        Ok(()) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({ "handle": args.handle, "outcome": "cancelling" })
                );
            } else {
                println!(
                    "Requested cancellation of batch `{}`. `kaibo batch get {}` for the final \
                     per-item results.",
                    args.handle, args.handle
                );
            }
            Ok(EXIT_OK)
        }
        Err(e) => Ok(fail_consultation(
            args.json,
            "batch cancel",
            backend_name,
            e,
        )),
    }
}

/// A stable string for a local cancel outcome in `--json`.
fn local_cancel_str(outcome: CancelLocalOutcome) -> &'static str {
    match outcome {
        CancelLocalOutcome::CancelledPending => "cancelled",
        CancelLocalOutcome::CancellingRunning => "cancelling",
        CancelLocalOutcome::AlreadyCancelled => "already_cancelled",
        CancelLocalOutcome::AlreadyFinished => "already_finished",
        CancelLocalOutcome::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_is_the_implicit_serve() {
        // No subcommand → the server path; the shared flags live on `cli.common`.
        let cli = Cli::try_parse_from(["kaibo", "--root", "/tmp"]).expect("bare parse");
        assert!(
            cli.command.is_none(),
            "bare kaibo has no subcommand (implicit serve)"
        );
        assert_eq!(
            cli.common.root.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    #[test]
    fn explicit_serve_takes_the_same_flags() {
        // `--cast` is a shared global (on cli.common); `--no-oneshot` is a serve gate.
        let cli = Cli::try_parse_from(["kaibo", "serve", "--no-oneshot", "--cast", "gemini"])
            .expect("serve parse");
        assert_eq!(cli.common.cast.as_deref(), Some("gemini"));
        match cli.command {
            Some(Command::Serve(gates)) => {
                assert!(gates.no_oneshot);
                assert!(gates.tool_disables().oneshot);
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn consult_subcommand_parses_its_flags() {
        let cli = Cli::try_parse_from([
            "kaibo",
            "consult",
            "why is this slow?",
            "--cast",
            "deepseek",
            "--attach",
            "a.rs",
            "--attach",
            "b.rs",
            "--session",
            "perf",
            "--json",
        ])
        .expect("consult parse");
        // `--cast` is the shared global; the rest are consult-specific.
        assert_eq!(cli.common.cast.as_deref(), Some("deepseek"));
        match cli.command {
            Some(Command::Consult(c)) => {
                assert_eq!(c.question, "why is this slow?");
                assert_eq!(c.attach, vec!["a.rs".to_string(), "b.rs".to_string()]);
                assert_eq!(c.session.as_deref(), Some("perf"));
                assert!(c.json);
            }
            other => panic!("expected consult, got {other:?}"),
        }
    }

    #[test]
    fn config_subcommand_parses() {
        let cli =
            Cli::try_parse_from(["kaibo", "config", "--root", "/srv/repo"]).expect("config parse");
        assert!(matches!(cli.command, Some(Command::Config)));
        // The shared `--root` global reaches the config subcommand.
        assert_eq!(
            cli.common.root.as_deref(),
            Some(std::path::Path::new("/srv/repo"))
        );
    }

    #[test]
    fn a_missing_consult_question_is_a_usage_error() {
        // The positional question is required — clap rejects its absence (exit 2 class).
        let err = Cli::try_parse_from(["kaibo", "consult"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    fn out_with(answer: &str, warnings: Vec<String>) -> ConsultOutput {
        ConsultOutput {
            answer: answer.to_string(),
            report: "the report".to_string(),
            usage: rig_core::completion::Usage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            warnings,
        }
    }

    #[test]
    fn consult_envelope_carries_answer_provenance_usage_and_warnings() {
        let out = out_with("the answer", vec![]);
        let env = consult_envelope(&out, "deepseek", "explorer-m", "synth-m", false);
        assert_eq!(env["answer"], "the answer");
        assert_eq!(env["cast"], "deepseek");
        assert_eq!(env["models"]["explorer"], "explorer-m");
        assert_eq!(env["models"]["synth"], "synth-m");
        assert_eq!(env["usage"]["input_tokens"], 10);
        assert_eq!(env["usage"]["output_tokens"], 20);
        // `warnings` is always present (empty when the turn was clean) so a consumer
        // can rely on the key.
        assert_eq!(env["warnings"], serde_json::json!([]));
        // The report is omitted unless requested.
        assert!(env.get("report").is_none(), "report is opt-in");

        let with_report = consult_envelope(&out, "deepseek", "e", "s", true);
        assert_eq!(with_report["report"], "the report");
    }

    /// A record-failure warning rides the `warnings` array — NOT the `answer` field, which
    /// stays the model's raw words (the #77 Gemini fix: `jq -r .answer` must be clean).
    #[test]
    fn consult_envelope_keeps_warnings_out_of_the_answer_field() {
        let out = out_with(
            "clean model words",
            vec!["⚠️ Session turn NOT recorded (persistence error: disk full).".to_string()],
        );
        let env = consult_envelope(&out, "deepseek", "e", "s", false);
        assert_eq!(
            env["answer"], "clean model words",
            "the answer field must be the model's raw words, no injected notices"
        );
        assert_eq!(env["warnings"][0], out.warnings[0]);
    }

    #[test]
    fn error_envelope_shape() {
        let env = error_envelope("consultation_failure", "it broke");
        assert_eq!(env["kind"], "consultation_failure");
        assert_eq!(env["error"], "it broke");
    }

    /// A global flag placed BEFORE the subcommand (`kaibo --cast x consult "q"`)
    /// propagates to the subcommand — the clap `global = true` contract on `CommonArgs`.
    #[test]
    fn a_global_flag_before_the_subcommand_reaches_it() {
        let cli = Cli::try_parse_from(["kaibo", "--cast", "gemini", "consult", "why?"])
            .expect("global flag before subcommand parses");
        assert_eq!(
            cli.common.cast.as_deref(),
            Some("gemini"),
            "a pre-subcommand --cast must parse and land on the shared common args"
        );
        match cli.command {
            Some(Command::Consult(c)) => assert_eq!(c.question, "why?"),
            other => panic!("expected consult, got {other:?}"),
        }
    }

    /// A batch/direct cast on interactive `consult` is a USAGE error (exit 2, kind
    /// "usage") — the offline-cast refusal must classify as usage, not a setup/containment
    /// rejection. Offline: `reject_offline_cast` fires before any model/key is touched.
    #[tokio::test]
    async fn an_offline_cast_on_consult_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let inline_budget = config.defaults.inline_attach_budget;
        let call_deadline = config.defaults.call_deadline;
        let et = config.defaults.explorer_max_turns;
        let st = config.defaults.synth_max_turns;
        let sandbox = config.sandbox.clone();
        let persistence = config.persistence.clone();
        let cap = config.defaults.session_capacity;
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        // `anthropic-batch` is a built-in whose synth runs on the batch lane; `--cast`
        // is a shared global, so it lands on `cli.common`.
        let cli = Cli::parse_from([
            "kaibo",
            "consult",
            "q",
            "--cast",
            "anthropic-batch",
            "--json",
        ]);
        let common = cli.common.clone();
        let args = match cli.command {
            Some(Command::Consult(c)) => c,
            _ => unreachable!("parsed a consult subcommand"),
        };

        let err = resolve_and_run(
            &common,
            &args,
            &resolver,
            inline_budget,
            call_deadline,
            et,
            st,
            &sandbox,
            &persistence,
            cap,
        )
        .await
        .expect_err("a batch cast on consult must be refused");
        assert_eq!(
            err.code, EXIT_USAGE,
            "offline-cast refusal is exit 2 (usage)"
        );
        assert_eq!(err.kind, "usage");
        // And the JSON envelope a script would see carries kind "usage".
        assert_eq!(error_envelope(err.kind, &err.message)["kind"], "usage");
    }

    // --- stage 2 subcommands -------------------------------------------------

    #[test]
    fn oneshot_explore_kaish_batch_subcommands_route() {
        // A global (--cast) before the subcommand still lands on cli.common.
        let cli = Cli::parse_from(["kaibo", "--cast", "gemini", "oneshot", "second opinion?"]);
        assert_eq!(cli.common.cast.as_deref(), Some("gemini"));
        match cli.command {
            Some(Command::Oneshot(o)) => {
                assert_eq!(o.prompt, "second opinion?");
                assert!(!o.json);
            }
            other => panic!("expected oneshot, got {other:?}"),
        }

        let cli = Cli::parse_from(["kaibo", "explore", "map the sandbox", "--json"]);
        match cli.command {
            Some(Command::Explore(e)) => {
                assert_eq!(e.question, "map the sandbox");
                assert!(e.json);
            }
            other => panic!("expected explore, got {other:?}"),
        }

        let cli = Cli::parse_from(["kaibo", "kaish", "-c", "grep -rn TODO src"]);
        match cli.command {
            Some(Command::Kaish(k)) => assert_eq!(k.command.as_deref(), Some("grep -rn TODO src")),
            other => panic!("expected kaish, got {other:?}"),
        }

        let cli = Cli::parse_from([
            "kaibo",
            "batch",
            "submit",
            "p1",
            "p2",
            "--cast",
            "gemini-batch",
        ]);
        assert_eq!(cli.common.cast.as_deref(), Some("gemini-batch"));
        match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Submit(s) => {
                    assert_eq!(s.prompts, vec!["p1".to_string(), "p2".to_string()])
                }
                other => panic!("expected batch submit, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_submit_requires_at_least_one_prompt() {
        let err = Cli::try_parse_from(["kaibo", "batch", "submit"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn batch_get_requires_a_handle() {
        let err = Cli::try_parse_from(["kaibo", "batch", "get"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn fold_stdin_context_appends_text_and_fails_loud_on_binary() {
        // No piped bytes → the bare prompt.
        assert_eq!(fold_stdin_context("ask", b"").unwrap(), "ask");
        // Whitespace-only stdin is ignored.
        assert_eq!(fold_stdin_context("ask", b"  \n\t ").unwrap(), "ask");
        // Text is appended after a blank line, trailing whitespace trimmed.
        assert_eq!(
            fold_stdin_context("ask", b"context here\n").unwrap(),
            "ask\n\ncontext here"
        );
        // Non-empty, non-UTF-8 (e.g. a piped PNG) is a LOUD usage error, never a silent
        // drop — the #77 DeepSeek fix.
        let err = fold_stdin_context("ask", &[0xff, 0xfe, 0x00, 0x01])
            .expect_err("binary stdin must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
        assert!(
            err.message.contains("isn't valid UTF-8"),
            "message names the cause: {}",
            err.message
        );
    }

    #[test]
    fn usage_json_carries_every_field() {
        let usage = rig_core::completion::Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..Default::default()
        };
        let u = usage_json(&usage);
        for k in [
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cached_input_tokens",
            "cache_creation_input_tokens",
        ] {
            assert!(u.get(k).is_some(), "usage_json must carry {k}");
        }
        assert_eq!(u["input_tokens"], 3);
        assert_eq!(u["output_tokens"], 4);
    }

    #[test]
    fn batch_poll_envelope_shapes_each_state() {
        use crate::batch::BatchAnswer;
        let pending = batch_poll_envelope(&BatchPoll::Pending {
            completed: 2,
            total: 5,
        });
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["completed"], 2);
        assert_eq!(pending["total"], 5);

        assert_eq!(
            batch_poll_envelope(&BatchPoll::Cancelling)["status"],
            "cancelling"
        );

        let done = batch_poll_envelope(&BatchPoll::Done(vec![
            BatchAnswer {
                custom_id: "0".into(),
                text: Ok("hi".into()),
            },
            BatchAnswer {
                custom_id: "1".into(),
                text: Err("boom".into()),
            },
        ]));
        assert_eq!(done["status"], "done");
        assert_eq!(done["answers"][0]["ok"], true);
        assert_eq!(done["answers"][0]["text"], "hi");
        assert_eq!(done["answers"][1]["ok"], false);
        assert_eq!(done["answers"][1]["error"], "boom");

        let failed = batch_poll_envelope(&BatchPoll::Failed {
            state: "expired".into(),
            message: "too late".into(),
        });
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["state"], "expired");
        assert_eq!(failed["message"], "too late");
    }

    #[test]
    fn batch_backends_scopes_and_refuses_a_non_batch_backend() {
        let resolver = Resolver::from_config(Arc::new(Config::builtin())).unwrap();
        // The built-in batch-capable backends (anthropic, gemini) — order-independent.
        let all = batch_backends(&resolver, None).expect("batch-capable backends exist");
        assert!(all.contains(&"anthropic".to_string()));
        assert!(all.contains(&"gemini".to_string()));
        assert!(
            !all.contains(&"deepseek".to_string()),
            "deepseek has no batch lane"
        );
        // An explicit non-batch backend is a usage error.
        let err = batch_backends(&resolver, Some("deepseek")).unwrap_err();
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
    }

    /// A non-batch cast on `batch submit` is a usage error (exit 2) — the offline
    /// `require_batch_cast` refusal, before any network. Mirrors the consult offline-cast test.
    #[tokio::test]
    async fn batch_submit_on_a_non_batch_cast_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        // `anthropic` is interactive (its synth is not on the batch lane).
        let cli = Cli::parse_from(["kaibo", "batch", "submit", "p1", "--cast", "anthropic"]);
        let common = cli.common.clone();
        let submit = match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Submit(s) => s,
                _ => unreachable!(),
            },
            _ => unreachable!("parsed a batch submit"),
        };
        let err = batch_submit_inner(&common, &submit, &resolver)
            .await
            .expect_err("an interactive cast on batch submit must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
    }

    #[test]
    fn a_missing_kaish_script_is_a_usage_error() {
        // clap accepts `kaibo kaish` with no -c (the arg is optional); the handler turns
        // the absence into a usage error pointing at -c, not a REPL.
        let cli = Cli::parse_from(["kaibo", "kaish"]);
        match cli.command {
            Some(Command::Kaish(k)) => assert!(k.command.is_none(), "-c is optional at parse time"),
            other => panic!("expected kaish, got {other:?}"),
        }
    }

    // --- local batch --------------------------------------------------------

    #[test]
    fn batch_submit_local_and_work_and_cancel_parse() {
        // `--local` on submit routes to the local lane; any cast is allowed.
        let cli = Cli::parse_from([
            "kaibo", "batch", "submit", "--local", "p1", "p2", "--cast", "deepseek",
        ]);
        match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Submit(s) => {
                    assert!(s.local, "--local sets the local flag");
                    assert_eq!(s.prompts, vec!["p1".to_string(), "p2".to_string()]);
                }
                other => panic!("expected submit, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        }

        // `batch work --watch` parses the optional poll interval.
        let cli = Cli::parse_from(["kaibo", "batch", "work", "--watch", "30"]);
        match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Work(w) => assert_eq!(w.watch, Some(30)),
                other => panic!("expected work, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        }

        // `batch cancel` takes a handle.
        let cli = Cli::parse_from(["kaibo", "batch", "cancel", "local/3"]);
        match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Cancel(c) => assert_eq!(c.handle, "local/3"),
                other => panic!("expected cancel, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_work_requires_no_positional() {
        // A bare `batch work` is valid (drain-then-exit).
        let cli = Cli::parse_from(["kaibo", "batch", "work"]);
        match cli.command {
            Some(Command::Batch(b)) => matches!(b.cmd, BatchCmd::Work(_)),
            _ => panic!("expected batch work"),
        };
    }

    #[test]
    fn local_job_envelope_shapes_each_item_state() {
        let job = LocalJob {
            id: 7,
            cast: "deepseek".into(),
            model: None,
            backend: None,
            status: LocalJobStatus::Done,
            created_at: 1,
            started_at: Some(2),
            finished_at: Some(3),
            attachments: vec![],
            items: vec![
                crate::store::LocalJobItem {
                    seq: 0,
                    prompt: "p0".into(),
                    result: Some(Ok("answer0".into())),
                    finished_at: Some(3),
                },
                crate::store::LocalJobItem {
                    seq: 1,
                    prompt: "p1".into(),
                    result: Some(Err("boom".into())),
                    finished_at: Some(3),
                },
                crate::store::LocalJobItem {
                    seq: 2,
                    prompt: "p2".into(),
                    result: None,
                    finished_at: None,
                },
            ],
        };
        let env = local_job_envelope(&job);
        assert_eq!(env["handle"], "local/7");
        assert_eq!(env["status"], "done");
        assert_eq!(env["cast"], "deepseek");
        assert_eq!(env["items"][0]["ok"], true);
        assert_eq!(env["items"][0]["text"], "answer0");
        assert_eq!(env["items"][1]["ok"], false);
        assert_eq!(env["items"][1]["error"], "boom");
        assert_eq!(env["items"][2]["pending"], true);
    }

    #[test]
    fn local_cancel_str_covers_every_outcome() {
        assert_eq!(
            local_cancel_str(CancelLocalOutcome::CancelledPending),
            "cancelled"
        );
        assert_eq!(
            local_cancel_str(CancelLocalOutcome::CancellingRunning),
            "cancelling"
        );
        assert_eq!(
            local_cancel_str(CancelLocalOutcome::AlreadyFinished),
            "already_finished"
        );
        assert_eq!(local_cancel_str(CancelLocalOutcome::Unknown), "unknown");
    }
}
