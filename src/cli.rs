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
//! - **exit codes have defined behavior.** `0` = an answer; `2` = a **usage** error — a bad
//!   argument, an unknown or wrong-for-the-tool cast, an image on a vision-blind cast
//!   (also clap's own arg-parse errors, and config load); `3` = a **setup/containment**
//!   rejection — a path or attachment outside the allowed set, a missing/unbuildable
//!   provider key; `4` = the work ran and failed at runtime (a provider/model-loop
//!   failure, or a `kaish` worker infra crash). So an agent branches on the code without
//!   parsing prose. (An arg-parse error is clap's: usage on stderr, exit 2, nothing on
//!   stdout — the envelope is guaranteed only once args parse. And `kaibo kaish` passes
//!   through kaish's own exit code on a normal run — 0 ok/1 failed/124 timed out/127
//!   not found.)
//!
//! `--help` is model-facing text: an agent reads it the way an MCP client reads a
//! tool description, so the top-level `about` front-loads what kaibo is and every
//! flag doc earns its line (the "Writing for models" discipline).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use rig_core::completion::Usage;
use rmcp::ErrorData as McpError;

use crate::batch::{BatchItem, BatchPoll};
use crate::config::{Config, Lane, ModelRole, ToolDisables};
use crate::consult::{
    batch_system_prompt, consult, deliberation_prompt, explore_with, oneshot as run_oneshot_engine,
    ConsultConfig, ConsultOutput, ExploreConfig, ModelCaps, PhaseContext,
};
use crate::progress::TerminalSink;
use crate::sandbox::KaishWorker;
use crate::server::{
    batch_within_window, configure_prompt_text_cli, consultation_failure_text, now_epoch_secs,
    render_config_resource, with_provenance, Resolver, BATCH_RECENCY_WINDOW_SECS,
    CONFIG_EXAMPLE_TOML,
};
use crate::session::{SessionStore as MemSessionStore, Sessions};
use crate::sweep_attach::{SweepAttachSink, SweepConsumer, SweepConsumerKind};

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

/// The `EXIT CODES` block appended to `kaibo --help` (long form only — `-h` stays a
/// terse usage line). This is the one place the taxonomy is spelled out for a human
/// or script reading `--help` cold, rather than the doc-comments above or the README.
const EXIT_CODES_HELP: &str = "\
EXIT CODES
    0  success — an answer, a report, or the requested output
    2  usage error — a bad argument, an unknown or wrong-for-the-tool cast, an
       image on a vision-blind cast, or a clap argument-parse error
    3  setup/containment rejection — a path or attachment outside the allowed
       set, a missing or unbuildable provider key
    4  the work ran and failed — a provider/model-loop failure, or a kaish
       worker infra crash

`kaibo kaish` is the one exception: it exits with kaish's own code instead of
this table (0 ok, 1 the command failed, 124 timed out, 127 command not found).
A refused write exits 1 and prints `permission denied: filesystem is
read-only` on stderr.";

/// kaibo — read-only codebase consultation from a model outside your own
/// family. Ask a question; a synthesis agent (DeepSeek, Gemini, Anthropic, OpenRouter,
/// or local — pick with `--cast`) reads the project and answers with `file:line`
/// citations. kaibo never writes to your project. Bare `kaibo` runs the MCP server on
/// stdio; `kaibo consult` answers one question and exits; `kaibo config` prints the
/// resolved configuration.
#[derive(Parser, Debug)]
#[command(name = "kaibo", version, after_long_help = EXIT_CODES_HELP)]
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
    /// Get a toolless second opinion: no codebase access, the model answers from your
    /// prompt (plus piped stdin and `--attach` files) and its own knowledge.
    Oneshot(OneshotArgs),
    /// Survey the codebase and print a cited report (the evidence half of consult).
    Explore(ExploreArgs),
    /// Investigate the codebase, then send that evidence to an offline synth model. A
    /// batch-lane cast prints a durable handle and exits; a direct-lane cast waits for a
    /// local model and prints the answer.
    Deliberate(DeliberateArgs),
    /// Run one kaish (sh-like) command against the read-only project and print its output.
    Kaish(KaishArgs),
    /// Provider batch lanes — submit many prompts at once, collect results, list handles.
    Batch(BatchArgs),
    /// Generate an image into kaibo's artifact store and print the digest that reads it
    /// back.
    Generate(GenerateCliArgs),
    /// Read an artifact kaibo stored, by its digest.
    Cas(CasArgs),
    /// List the models a backend serves, read from its own /models endpoint. No cast is
    /// resolved and no model runs.
    Models(ModelsArgs),
    /// Print the resolved runtime configuration (the `kaibo://config` document).
    Config,
    /// Print the guided "set up my models" walkthrough — the CLI equivalent of the
    /// `configure` MCP prompt, for a CLI-driving agent or a human with no MCP prompt
    /// support. Never writes config.toml itself; the caller does, with its own access.
    Configure(ConfigureArgs),
    /// Print the annotated config.toml template (the `kaibo://config/example` document).
    ExampleConfig,
}

/// `kaibo configure` — the same guided "set up my models" walkthrough as the
/// `configure` MCP prompt, framed for a caller with no MCP resource access.
#[derive(Args, Debug)]
pub struct ConfigureArgs {
    /// What you want from the setup, e.g. "a local-only privacy cast" or "a cheap
    /// DeepSeek explorer with a Claude synth". Optional — omit for a general walkthrough.
    pub goal: Option<String>,
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
    /// workspace and you configure nothing. Not repeatable: this names *the* project a
    /// path-less call defaults to, and there can only be one — `--allow-path` is the
    /// repeatable, additive knob. Setting it also means the cwd is NOT added.
    #[arg(long, value_name = "DIR", global = true)]
    pub root: Option<PathBuf>,

    /// Additional allowed path tree. Repeatable, and purely ADDITIVE — it widens the
    /// boundary and never narrows it, so without --root the invocation cwd stays
    /// allowed alongside it (use --no-cwd to drop that). Use `--allow-path /` to lift
    /// all limits. Also settable via KAIBO_ALLOW_PATHS (colon-separated) or
    /// [server] allow_paths; a non-empty set of flags replaces that layer.
    #[arg(long = "allow-path", value_name = "DIR", action = clap::ArgAction::Append, global = true)]
    pub allow_path: Vec<PathBuf>,

    /// Don't adopt the invocation cwd as an allowed tree and default root. By default
    /// it is one (unless --root is given), and --allow-path only ever ADDS to that.
    /// With this, the allowed set is exactly what you named and every call must pass
    /// its own path. Also KAIBO_NO_CWD.
    #[arg(long = "no-cwd", global = true)]
    pub no_cwd: bool,

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

    /// Don't persist sessions or batch handles — run fully in memory. By default kaibo
    /// keeps a state db so a `--session` survives a restart and is shared between the CLI
    /// and the MCP server. Also KAIBO_NO_PERSISTENCE.
    #[arg(long, global = true)]
    pub no_persistence: bool,

    /// Path to the persistence state db. Overrides KAIBO_STATE_DB / [persistence] path
    /// / the default ($XDG_STATE_HOME/kaibo/state.db).
    #[arg(long = "state-db", value_name = "FILE", global = true)]
    pub state_db: Option<PathBuf>,

    /// Directory for the artifact store (generated images and saved output). Overrides
    /// KAIBO_CAS_DIR /
    /// [cas] dir / the default ($XDG_DATA_HOME/kaibo/cas).
    #[arg(long = "cas-dir", value_name = "DIR", global = true)]
    pub cas_dir: Option<PathBuf>,

    /// Cap on total artifact-store size, in bytes. A write that would exceed it is
    /// refused and nothing is deleted — kaibo never evicts an artifact you paid for.
    /// Unset (the default) means no cap and no size accounting; setting one makes every
    /// new write sum the whole store first. Overrides KAIBO_CAS_MAX_BYTES /
    /// [cas] max_bytes.
    #[arg(long = "cas-max-bytes", value_name = "BYTES", global = true)]
    pub cas_max_bytes: Option<u64>,

    /// Cap on how many files one explorer sweep may attach with its `attach` tool;
    /// default 32. The attached files are sent to whatever reads that sweep's report:
    /// the consult's synth model, or deliberate's offline synth. `0` removes the tool
    /// entirely. Also KAIBO_MAX_ATTACHMENTS / [defaults] max_attachments.
    #[arg(long = "max-attachments", value_name = "N", global = true)]
    pub max_attachments: Option<usize>,
}

/// The per-tool gates — serve-only (they only make sense for the long-lived server).
/// The shared flags live on [`Cli::common`] as globals.
///
/// All but one are `--no-<tool>`, which can only narrow what the server advertises.
/// `--allow-save-artifact` runs the other way: it is the operator key for a capability
/// whose built-in default is off. See [`crate::config::ArtifactsConfig`].
#[derive(Args, Debug, Clone, Default)]
pub struct ServeGates {
    /// Don't advertise the `consult` tool. This also drops `consult_submit` and the job
    /// verbs that only `consult` produces.
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
    /// Don't advertise the `list_models` tool.
    #[arg(long)]
    pub no_list_models: bool,
    /// Don't advertise the `generate` tool (media generation).
    #[arg(long)]
    pub no_generate: bool,

    /// Let a consult save bulk output as artifacts in kaibo's artifact store, when the
    /// call also asks for it with `save_artifacts` (both keys are required, and the
    /// artifact store must be on). OFF by default — this is the one capability a model,
    /// rather than the operator, decides to use. Also KAIBO_ARTIFACTS_ENABLED /
    /// [artifacts] enabled.
    #[arg(long = "allow-save-artifact")]
    pub allow_save_artifact: bool,
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
            list_models: self.no_list_models,
            generate: self.no_generate,
        }
    }
}

/// `kaibo consult` — one read-only consultation. The shared flags come from
/// [`Cli::common`] (globals); these are the consult-specific ones.
#[derive(Args, Debug)]
pub struct ConsultArgs {
    /// The question to investigate. Say in prose what you did or want to know — kaibo
    /// locates and reads the real, current code itself, so describe the change in your
    /// own words instead of pasting a diff.
    pub question: String,

    /// Project to explore. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Workspace file to give the investigation. Files are inlined into the prompt in
    /// the order you pass them, while the `[defaults] inline_attach_budget` lasts (262144
    /// bytes by default); past that the model is directed to read the file whole instead.
    /// An image needs a vision-capable cast. Repeatable.
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

    /// Also print the explorer's aggregated report. Under --json it is the `report`
    /// field on stdout; otherwise it goes to stderr under a `--- explorer report ---`
    /// header, so a pipe still captures only the answer. Empty when the consult
    /// delegated no sweep.
    #[arg(long)]
    pub include_report: bool,

    /// Emit a JSON envelope on stdout (answer + provenance + usage + warnings) instead
    /// of prose, for a script caller. A failure emits `{"error": …, "kind": …}` instead,
    /// where `kind` is one of `config`, `usage`, `setup`, `persistence`, or
    /// `consultation_failure`. An argument-parse error is the exception: usage goes to
    /// stderr and the process exits 2 with nothing on stdout, so the envelope is
    /// guaranteed only once the arguments parse.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo oneshot` — a toolless second opinion from a model outside your family. No
/// codebase access: the model answers from your prompt (plus any context piped on
/// stdin, `… < notes.md`) and `--attach`ed files. Pick the answering team with `--cast`.
#[derive(Args, Debug)]
pub struct OneshotArgs {
    /// The prompt to send the model. Context piped on stdin is appended, as in
    /// `kaibo oneshot "review this" < diff.txt`. Piped input must be text; non-text input
    /// is refused with exit 2. No codebase access, so include (or `--attach`) whatever
    /// the answer needs.
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
    /// What to survey or map. Say in prose what you want mapped — kaibo's explorer
    /// locates and reads the real, current code and reports back with citations.
    pub question: String,

    /// Project to explore. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Workspace file central to the survey: the explorer is directed to read it
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

/// `kaibo deliberate` — investigate, then reason offline over the evidence.
///
/// The two lanes end differently, and that is the whole shape of this command. A **batch**
/// cast submits to the provider and prints a durable handle: the work outlives this
/// process, and `kaibo batch get <handle>` collects it whenever. A **direct** cast has no
/// handle to give — the job would be this process — so it runs the long local completion
/// in the foreground and prints the answer, which is the CLI's usual contract. Either way
/// the dossier is kept, and `--dossier` hands a kept one to a second cast with no sweep.
#[derive(Args, Debug)]
pub struct DeliberateArgs {
    /// The hard question to reason through. Say in prose what you want deliberated —
    /// kaibo's explorer locates and reads the real, current code to build the dossier the
    /// offline synth then reasons over.
    pub question: String,

    /// Project to investigate. Optional — defaults to the root/allowed cwd; must resolve
    /// to at-or-under an allowed tree.
    #[arg(long, value_name = "DIR")]
    pub path: Option<String>,

    /// Workspace file central to the question: the dossier-building explorer is directed
    /// to read it WHOLE, so its content reaches the offline synth through the dossier.
    /// Text only. Repeatable.
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<String>,

    /// Reuse a dossier kaibo already kept instead of sweeping for a new one: the digest
    /// an earlier deliberation printed, bare or as its `kaibo://cas/<digest>` URI. No
    /// explorer runs, so this costs one synth — the way to put the same evidence in front
    /// of a second cast. The explorer flags below have nothing to act on with it and are
    /// refused rather than ignored.
    #[arg(long, value_name = "DIGEST")]
    pub dossier: Option<String>,

    /// Override the explorer (dossier-building) model id (verbatim; pair with
    /// --explorer-backend to also retarget).
    #[arg(long, value_name = "ID")]
    pub explorer_model: Option<String>,
    /// Run the explorer override on this backend (name or alias). Requires --explorer-model.
    #[arg(long, value_name = "BACKEND")]
    pub explorer_backend: Option<String>,
    /// Override the synth (deliberating) model id. Its lane — batch or direct — still
    /// comes from the cast. Pair with --synth-backend to also retarget.
    #[arg(long, value_name = "ID")]
    pub synth_model: Option<String>,
    /// Run the synth override on this backend (name or alias). Requires --synth-model.
    #[arg(long, value_name = "BACKEND")]
    pub synth_backend: Option<String>,
    /// Max tool-loop turns for the dossier-building explorer sweep (default 100).
    #[arg(long, value_name = "N")]
    pub explorer_max_turns: Option<usize>,

    /// Emit a JSON envelope on stdout instead of prose. A batch cast's envelope carries
    /// the handle to collect later; a direct cast's carries the answer it waited for.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo kaish` — one non-interactive kaish command through the same READ-ONLY sandbox
/// the `run_kaish` MCP tool uses. Scriptable single execution only: no readline, no
/// REPL. The process exits with kaish's own exit code (0 ok, 1 the command failed,
/// 124 timed out, 127 command not found).
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

/// `kaibo generate` — render an image through the cast's `image` slot into kaibo's own
/// artifact store, and print the digest that reads it back.
///
/// Never writes into your project: the address is the content's hash and the store is
/// kaibo's, which is the same contract the MCP tool holds. `kaibo cas read <digest>`
/// fetches it, and in disk mode the printed path reaches it with any tool you like.
#[derive(Args, Debug)]
pub struct GenerateCliArgs {
    /// What to generate, described in prose.
    pub prompt: String,

    /// A provider-native option, `key=value`, passed through verbatim. Repeatable. The
    /// value is read as JSON when it parses as one, so `--field n=2` sends a number and
    /// `--field transparent=true` a boolean; anything else rides as a string
    /// (`--field size=1024x1024`). The provider validates its own knobs. `prompt` and
    /// `model` are reserved — use the argument and the cast's image slot.
    #[arg(long = "field", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub fields: Vec<String>,

    /// Emit a JSON envelope on stdout (the artifacts and their digests) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo cas` — store an image, and read back what kaibo stored, by address.
///
/// `read` and `write` are the only verbs, on purpose. The store is content-addressed and
/// opaque: you reach an object with a digest you already hold, and storing one is how you
/// come to hold it. There is no listing, no usage report,
/// and no delete, because none of those can exist without an index the store does not
/// keep. kaibo never deletes an artifact — to reclaim space, delete files under the store
/// directory yourself by mtime; `kaibo config` prints that path.
#[derive(Args, Debug)]
pub struct CasArgs {
    #[command(subcommand)]
    pub cmd: CasCmd,
}

#[derive(Subcommand, Debug)]
pub enum CasCmd {
    /// Read one artifact by digest — metadata first, then a bounded window of content.
    Read(CasReadArgs),
    /// Store an image file and print its digest.
    Write(CasWriteArgs),
}

/// `kaibo cas read` — metadata first, then a bounded window of content. The same rules
/// the `read_cas` MCP tool applies.
#[derive(Args, Debug)]
pub struct CasReadArgs {
    /// The artifact's digest, bare or as its full `kaibo://cas/<digest>` URI — whichever
    /// kaibo printed.
    pub digest: String,

    /// Start reading at this byte offset (default 0). Every read's metadata names the
    /// total size, so you can page without guessing.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub offset: usize,

    /// How many bytes to serve. Omitted, a text object gives a 65536-byte window from
    /// `offset` and a binary object gives metadata alone; `0` is metadata only for any
    /// object. A length above 1048576 bytes is refused rather than trimmed, so your next
    /// `offset` is never wrong.
    #[arg(long, value_name = "N")]
    pub length: Option<usize>,

    /// Emit a JSON envelope on stdout (metadata + the served body) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo cas write` — store an image and print its digest. The command-line half of the
/// `write_cas` MCP tool, and the same admission rules: the format is read from the bytes,
/// and an image over 8388608 bytes is refused rather than trimmed.
///
/// There is no base64 input here and no allowed-set check on the path. Both differ from
/// the MCP tool deliberately, and the path one deserves its reason stated plainly, because
/// `upload.rs` argues the opposite for its own caller and a reader will find that first:
/// **that tool's caller is a model that can be prompt-injected**, so an attacker-authored
/// instruction to pull `~/.ssh/id_rsa` into the store and read it back is a real shape the
/// boundary refuses. This command's caller is the operator, running with their own
/// privileges, naming a file they can already `cat`. Scoping it would be friction with
/// nothing on the other side of it. The bytes differ for a duller reason: an MCP caller may
/// hold an image that was never a file, and a shell user has a path.
#[derive(Args, Debug)]
pub struct CasWriteArgs {
    /// The image file to store. png, jpeg, gif or webp — the format is read from the
    /// file's own bytes, so there is nothing to declare and nothing to get wrong.
    pub path: PathBuf,

    /// One short line describing the image, recorded beside it and shown by
    /// `kaibo cas read`. At most 200 bytes, single-line.
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,

    /// Emit a JSON object on stdout — the digest and its metadata — instead of the bare
    /// digest.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo batch` — the provider batch lanes. Work runs offline at the provider and
/// returns hours later, typically at half the interactive price. Same behavior as the
/// MCP batch verbs.
#[derive(Args, Debug)]
pub struct BatchArgs {
    #[command(subcommand)]
    pub cmd: BatchCmd,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BatchCmd {
    /// Submit self-contained prompts to a batch cast; prints the durable
    /// `backend/provider-id` handle.
    Submit(BatchSubmitArgs),
    /// Fetch a batch by handle — a progress line while it runs, per-item answers when done.
    Get(BatchGetArgs),
    /// List batches the providers still know about, plus handles recovered from the store.
    List(BatchListArgs),
    /// Ask the provider to stop a batch. Already-finished items still collect with `get`.
    Cancel(BatchGetArgs),
}

/// `kaibo batch submit` — like `oneshot`, no tools/codebase access: each prompt carries
/// its own context (or shared `--attach` files). Needs a batch-capable cast/backend.
#[derive(Args, Debug)]
pub struct BatchSubmitArgs {
    /// The prompts to submit, one batch item each. At least one required.
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

    /// Emit a JSON object `{handle, cast, model, count}` instead of the bare handle.
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
    /// Which backend (name or alias) to list. Omit to query every batch-capable backend.
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Show all batches, including ones older than 24h (trimmed by default).
    #[arg(long)]
    pub all: bool,

    /// Emit a JSON object (entries + recovered handles + per-backend errors) instead of prose.
    #[arg(long)]
    pub json: bool,
}

/// `kaibo models` — read-only model discovery: ask a backend's provider "what models
/// do you have" through kaibo's already-configured auth, instead of hand-rolling a curl.
/// No cast, no model in the loop — a pure operator/config query.
#[derive(Args, Debug)]
pub struct ModelsArgs {
    /// Which backend (name or alias) to query. Omit to query every configured backend.
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Emit a JSON object (per-backend models, normalized fields + raw passthrough)
    /// instead of prose.
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
        common.no_cwd,
        common.project_context_file.clone(),
        common.user_context_file.clone(),
        common.no_persistence,
        common.state_db.clone(),
        common.cas_dir.clone(),
        common.cas_max_bytes,
        // The CLI front doors have no client key to pair with the operator key —
        // `save_artifacts` is an MCP `consult` argument — so a CLI consult never gets the
        // tool, and passing `true` here would be a promise nothing keeps. It stays off
        // rather than silently inert (`--allow-save-artifact` is a serve-only gate on
        // `ServeGates` for exactly this reason).
        false,
        common.max_attachments,
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

/// [`init_cli_logging`], plus the OTLP layers `[telemetry]` asked for — the CLI's
/// counterpart to `serve()`'s subscriber build in `main.rs`. Installing both layers
/// in one `try_init()` call here means a subcommand's own later `init_cli_logging()`
/// call (every `run_*` fn makes one, for callers that reach it without going through
/// `main`) simply no-ops — `try_init` is idempotent, and the global default this
/// function installed already carries both the stderr sink and the exporter.
fn init_cli_logging_with_otel(
    otel_layers: Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
    >,
) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(otel_layers)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter),
        )
        .try_init();
}

/// Stand up OTLP for a CLI subcommand when `[telemetry]` is configured — Amy's ask:
/// "CLI should try to do otel if it has connection info." Mirrors `serve()` in
/// `main.rs`: build the exporter layers, install them alongside the CLI's normal
/// stderr logging, and hand the guard back so `main` can flush and shut it down.
///
/// **`main` must hold the returned guard and call
/// [`shutdown`](crate::telemetry::OtelGuard::shutdown) AFTER the subcommand returns
/// and BEFORE `std::process::exit`.** Every CLI arm exits through
/// `std::process::exit`, which skips destructors — a guard merely dropped here would
/// discard whatever the batch processor hadn't exported yet, silently losing every
/// span from a short CLI run. This is called once, before dispatch, so it does not
/// know which subcommand is about to run; the OTel layers export whatever spans that
/// subcommand's own instrumentation produces (rig's GenAI tree for a model phase,
/// kaish-kernel's command spans for `kaish`, ...).
///
/// A config-load failure here returns `Ok(None)` rather than propagating: telemetry
/// is off in that case the same as if it were never configured, and the *real* error
/// surfaces a moment later when the subcommand makes its own `load_config` call and
/// reports it through the normal `--json`/stderr preflight path — swallowing it here
/// only defers where it's told, it never hides it. A telemetry-specific failure
/// (config loads fine but the exporter itself won't build — e.g. a logs endpoint
/// that can't be derived) is different: nothing else re-checks that, so it propagates
/// loudly, same as the fatal build error on the `serve` path.
pub async fn init_cli_telemetry(
    common: &CommonArgs,
) -> anyhow::Result<Option<crate::telemetry::OtelGuard>> {
    let Ok(config) = load_config(common) else {
        return Ok(None);
    };
    if !config.telemetry.enabled {
        return Ok(None);
    }
    match crate::telemetry::init::<tracing_subscriber::Registry>(&config.telemetry)? {
        Some((layers, guard)) => {
            init_cli_logging_with_otel(layers);
            Ok(Some(guard))
        }
        // `enabled` is checked above, so `init` cannot return `None` here — kept as a
        // match arm rather than `.expect()` because the type is still `Option`.
        None => Ok(None),
    }
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
            max_attachments: resolver.config.defaults.max_attachments,
        },
        synth_max_turns: args.synth_max_turns.unwrap_or(default_synth_turns),
        attachments,
        // No client key on this front door (see `load_config`), so no sink and no
        // `save_artifact` in the CLI consult's toolset.
        artifacts: None,
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
        message: "kaibo cannot start a `--session`: persistence is on but no state db path is \
             set. Name one with `--state-db FILE`, set `[persistence] path` in config.toml, \
             or pass `--no-persistence` to keep this session in memory for this run only."
            .to_string(),
        code: EXIT_USAGE,
    })?;
    let allowed = resolver.allowed_set();
    let allowed_refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
    let store = crate::store::SessionStore::open(&path, session_capacity, &allowed_refs)
        .await
        .map_err(|e| SetupError {
            kind: "persistence",
            message: format!(
                "kaibo could not open the persistence state db at {}: {e:#}. Fix the \
                 path or its permissions, or name another with --state-db. Or pass \
                 --no-persistence to keep this `--session` in memory for this run only — \
                 the session works now, and is lost when the process exits, so it is never \
                 shared with the MCP server.",
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
    // a consult here would activate, so report that. The CAS mode is predicted from
    // the same assumption, through the one shared derivation (`Config::cas_mode`).
    let cas_mode = resolver.config.cas_mode(persistence_enabled);
    // Probe the backing filesystem here too, on the same prediction. `kaibo config` is
    // the natural place an operator checks *before* spending anything, and answering
    // "disk" without saying the disk is a container's overlay would make this the one
    // surface that still hides it. Read-only and cheap; only meaningful in disk mode.
    let cas_ephemeral_fs = match (cas_mode, resolver.config.cas.dir.as_deref()) {
        (crate::config::CasMode::Disk, Some(dir)) => crate::cas::probe_backing(dir).ephemeral_fs(),
        _ => None,
    };
    let body = render_config_resource(
        &resolver.config,
        resolver.allowed_trees(),
        resolver.default_root_ref(),
        resolver.default_root_inferred(),
        resolver.followed_worktrees(),
        persistence_enabled,
        cas_mode,
        cas_ephemeral_fs,
    );
    println!("{body}");
    EXIT_OK
}

/// Run `kaibo configure`: print the same guided "set up my models" walkthrough the
/// `configure` MCP prompt gives an agent — the roster-design guidance is the exact
/// same shared text ([`configure_prompt_text_cli`]), just wrapped in framing that
/// points at `kaibo example-config`/`kaibo config` instead of MCP resource URIs a
/// CLI-only caller may not be able to reach. kaibo's CLI never writes `config.toml`
/// itself, same as the MCP prompt — the caller (a CLI-driving agent, or a human) reads
/// this and writes the file with its own access.
pub fn run_configure(goal: Option<String>) -> i32 {
    println!("{}", configure_prompt_text_cli(goal.as_deref()));
    EXIT_OK
}

/// Run `kaibo example-config`: print the annotated config.toml template — the same
/// embedded string the `kaibo://config/example` MCP resource serves, so the two never
/// drift.
pub fn run_example_config() -> i32 {
    println!("{CONFIG_EXAMPLE_TOML}");
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
        max_attachments: resolver.config.defaults.max_attachments,
    };
    match explore_with(&args.question, root, &explorer, &cfg, &attachments, None).await {
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
// deliberate
// ---------------------------------------------------------------------------

/// Settle this run's media CAS, the same three ways the MCP server does
/// ([`crate::cas::open_media_store`]).
///
/// `persistence_active` is what the durable store actually did, not what config asked
/// for — the identical rule the server applies, so the same install lands in the same
/// mode on either road. A disk-open failure is fatal here too: kaibo never quietly
/// downgrades to a memory store that loses what you paid for.
fn open_media_cas(
    resolver: &Resolver,
    persistence_active: bool,
) -> Result<Option<Arc<crate::cas::MediaStore>>, SetupError> {
    let allowed = resolver.allowed_set();
    let refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
    let (store, _ephemeral) = crate::cas::open_media_store(
        &resolver.config.cas,
        resolver.config.cas_mode(persistence_active),
        &refs,
        crate::cas::probe_backing,
    )
    .map_err(|e| SetupError {
        kind: "setup",
        message: format!("{e:#}"),
        code: EXIT_SETUP,
    })?;
    Ok(store.map(Arc::new))
}

/// Run `kaibo deliberate` — investigate, then reason offline over the evidence.
///
/// Which lane the cast carries decides what this process does with its life, and the two
/// answers are honest opposites: a batch cast prints a durable handle and exits (the work
/// is the provider's now), a direct cast waits for the local model and prints the answer
/// (this process IS the job — there is nothing else to hold it). Issue #82 held the
/// direct lane off the CLI while a `job-N` looked like the only shape; it isn't.
pub async fn run_deliberate(common: CommonArgs, args: DeliberateArgs) -> i32 {
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
    match deliberate_inner(&common, &args, &resolver).await {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(args.json, kind, message, code),
    }
}

async fn deliberate_inner(
    common: &CommonArgs,
    args: &DeliberateArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    // A reuse call carrying explorer flags is self-contradictory; say so before resolving
    // anything, so it costs nothing. Shared with the MCP handler.
    if let Some(refusal) = crate::server::inert_explorer_args(crate::server::ExplorerArgs {
        dossier: args.dossier.as_deref(),
        attach: &args.attach,
        model: args.explorer_model.as_deref(),
        backend: args.explorer_backend.as_deref(),
        max_turns: args.explorer_max_turns,
    }) {
        return Err(SetupError {
            kind: "usage",
            message: refusal,
            code: EXIT_USAGE,
        });
    }
    let root = resolver
        .resolve_root(args.path.clone())
        .map_err(SetupError::setup)?;
    let mut cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    // deliberate = investigate → OFFLINE synth. Capture the lane from the chosen cast
    // BEFORE any override runs: an override retargets the model, never the mechanism, and
    // `apply_model_override` replaces the slot with a laneless one (see the MCP handler's
    // `deliberation_lane_with_overrides` for the invariant this mirrors).
    let lane = cast.synth_lane().ok_or_else(|| SetupError {
        kind: "usage",
        message: format!(
            "cast `{}` has no offline synth — `deliberate` needs a cast pairing an \
             explorer with a synth on the `batch` or `direct` lane (the example config's \
             `fable`/`gemini-deliberate`/`local-direct`, or your own). For an answer this \
             turn, use `kaibo consult`.",
            cast.name
        ),
        code: EXIT_USAGE,
    })?;
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
    let system = batch_system_prompt(resolver.resolved_prompts(&cast).batch.as_deref());

    // The durable store settles two things at once: whether a batch handle can be
    // recorded, and (as the server's own rule) whether the CAS is disk or memory.
    let store = open_batch_store(resolver).await;
    let media = open_media_cas(resolver, store.is_some())?;

    // Stage 1 — build the dossier, or reuse one already kept.
    let (dossier, images, dossier_usage, kept, explorer_model) = if let Some(reference) =
        args.dossier.as_deref()
    {
        let (text, kept) =
            crate::server::load_dossier(media.as_ref(), reference).map_err(|message| {
                SetupError {
                    kind: "usage",
                    message,
                    code: EXIT_USAGE,
                }
            })?;
        (text, Vec::new(), Usage::new(), Some(kept), None)
    } else {
        let explorer = resolver
            .arm(&cast, ModelRole::Explorer)
            .map_err(SetupError::setup)?;
        let explorer_model = explorer.model.clone();
        let attachments = resolver
            .resolve_sweep_attachments(&root, &args.attach, "deliberate")
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
            max_attachments: resolver.config.defaults.max_attachments,
        };
        // The offline synth's caps decide what the sweep may route to it — resolved
        // key-free, before the sweep spends anything, exactly as the MCP handler does.
        let synth_slot = cast
            .require_slot(ModelRole::Synth)
            .map_err(|e| SetupError {
                kind: "usage",
                message: e.to_string(),
                code: EXIT_USAGE,
            })?;
        let synth_backend = resolver
            .config
            .resolve_backend(&synth_slot.backend)
            .map_err(|e| SetupError {
                kind: "setup",
                message: e.to_string(),
                code: EXIT_SETUP,
            })?;
        let synth_caps = ModelCaps::resolve(synth_backend.kind, &synth_slot.id, synth_slot.vision);
        let consumer = SweepConsumer {
            kind: SweepConsumerKind::OfflineSynth,
            label: Arc::from(format!(
                "the offline synth (`{}`) on cast `{}`",
                synth_slot.id, cast.name
            )),
            vision: synth_caps.vision,
        };
        let sink = (resolver.config.defaults.max_attachments > 0).then(|| {
            Arc::new(SweepAttachSink::new(
                resolver.config.defaults.max_attachments,
                consumer.clone(),
                std::collections::HashSet::new(),
            ))
        });
        let (mut dossier, usage) = match explore_with(
            &args.question,
            root,
            &explorer,
            &cfg,
            &attachments,
            sink.as_ref(),
        )
        .await
        {
            Ok(out) => out,
            Err(e) => return Ok(fail_consultation(args.json, "deliberate", &cast.name, e)),
        };
        let (images, kept) = crate::server::stitch_and_keep(
            &mut dossier,
            &consumer,
            sink.as_ref(),
            media.as_ref(),
            &args.question,
            &cast.name,
            &explorer_model,
        );
        (dossier, images, usage, kept, Some(explorer_model))
    };

    // Stage 2 — the lane decides how this process ends.
    match lane {
        Lane::Batch => {
            deliberate_batch_cli(
                args,
                resolver,
                &cast,
                &dossier,
                &images,
                &system,
                dossier_usage,
                kept.as_ref(),
                explorer_model.as_deref(),
                store,
            )
            .await
        }
        Lane::Direct => {
            deliberate_direct_cli(
                args,
                resolver,
                &cast,
                &dossier,
                &images,
                &system,
                dossier_usage,
                kept.as_ref(),
                explorer_model.as_deref(),
            )
            .await
        }
    }
}

/// Stage 2, batch lane: submit and hand back the durable handle. The work outlives this
/// process — `kaibo batch get <handle>` collects it, exactly as it collects a
/// `kaibo batch submit`. stdout carries the handle alone so a script can pipe it.
#[allow(clippy::too_many_arguments)] // each argument is a distinct, named stage-2 input
async fn deliberate_batch_cli(
    args: &DeliberateArgs,
    resolver: &Resolver,
    cast: &crate::config::Cast,
    dossier: &str,
    images: &[crate::attach::Attachment],
    system: &str,
    dossier_usage: Usage,
    kept: Option<&crate::server::KeptDossier>,
    explorer_model: Option<&str>,
    store: Option<crate::store::SessionStore>,
) -> Result<i32, SetupError> {
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
    let backend_name = backend.name.clone();
    let model = slot.id.clone();
    let provider =
        crate::batch::submitter(backend, slot, &resolver.config.defaults).map_err(|e| {
            SetupError {
                kind: "setup",
                message: format!("{e:#}"),
                code: EXIT_SETUP,
            }
        })?;
    let items = vec![BatchItem {
        custom_id: "0".to_string(),
        prompt: deliberation_prompt(&args.question, dossier),
    }];
    let provider_id = match provider.submit(system, images, &items).await {
        Ok(id) => id,
        Err(e) => return Ok(fail_consultation(args.json, "deliberate", &cast.name, e)),
    };
    let handle = format!("{backend_name}/{provider_id}");
    // Persist the handle with the dossier's address on its label — the same record the
    // MCP handler writes, so a handle recovered by `kaibo batch list` leads back to the
    // evidence whichever front door launched it.
    if let Some(store) = store {
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
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "handle": handle,
                "cast": cast.name,
                "models": {"explorer": explorer_model, "synth": model},
                "dossier": kept.map(|k| k.digest.clone()),
                "usage": usage_json(&dossier_usage),
            })
        );
    } else {
        println!("{handle}");
        let stage_one = match explorer_model {
            Some(m) => format!("dossier built (explorer `{m}`)"),
            None => "dossier reused (no explorer ran)".to_string(),
        };
        eprintln!(
            "kaibo: {stage_one}, handed to the batch lane on cast `{}` (synth `{model}`) — \
             `kaibo batch get {handle}` for the deliberation.{}",
            cast.name,
            crate::server::dossier_ack(kept)
        );
    }
    Ok(EXIT_OK)
}

/// Stage 2, direct lane: run the long local completion in the FOREGROUND and print the
/// answer.
///
/// The MCP server hands this lane a session-scoped `job-N` because it has a process to
/// hold the job open. A one-shot CLI invocation does not — it would submit, exit, and take
/// the job with it (issue #82). So the process *is* the job: it waits, prints, and exits,
/// which is also the CLI's ordinary contract. The wall-clock backstop is the synth
/// backend's own `request_timeout`, shared with the MCP lane, because a local model that
/// takes three hours is the point rather than a hang.
#[allow(clippy::too_many_arguments)] // each argument is a distinct, named stage-2 input
async fn deliberate_direct_cli(
    args: &DeliberateArgs,
    resolver: &Resolver,
    cast: &crate::config::Cast,
    dossier: &str,
    images: &[crate::attach::Attachment],
    system: &str,
    dossier_usage: Usage,
    kept: Option<&crate::server::KeptDossier>,
    explorer_model: Option<&str>,
) -> Result<i32, SetupError> {
    let synth = resolver
        .arm(cast, ModelRole::Synth)
        .map_err(SetupError::setup)?;
    let synth_slot = cast
        .require_slot(ModelRole::Synth)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    let synth_backend = resolver
        .config
        .resolve_backend(&synth_slot.backend)
        .map_err(|e| SetupError {
            kind: "setup",
            message: e.to_string(),
            code: EXIT_SETUP,
        })?;
    let deadline = crate::server::deliberate_direct_deadline(synth_backend);
    // Say what is about to happen before it happens: this blocks for as long as the local
    // model needs, and a caller staring at a silent terminal deserves to know why.
    eprintln!(
        "kaibo: deliberating on cast `{}` (direct synth `{}`) — one long local completion, \
         this process waits for it.",
        cast.name, synth.model
    );
    match crate::consult::deliberate_direct(
        &args.question,
        dossier,
        images,
        &synth,
        system,
        deadline,
    )
    .await
    {
        Ok((answer, synth_usage)) => {
            let usage = dossier_usage + synth_usage;
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "answer": answer,
                        "cast": cast.name,
                        "models": {"explorer": explorer_model, "synth": synth.model},
                        "dossier": kept.map(|k| k.digest.clone()),
                        "usage": usage_json(&usage),
                    })
                );
            } else {
                let mut roles: Vec<(&str, &str)> = Vec::with_capacity(2);
                if let Some(m) = explorer_model {
                    roles.push(("explorer", m));
                }
                roles.push(("synth", &synth.model));
                println!(
                    "{}",
                    with_provenance(
                        crate::server::with_dossier(answer, kept),
                        &cast.name,
                        &roles,
                        &usage
                    )
                );
            }
            Ok(EXIT_OK)
        }
        Err(e) => Ok(fail_consultation(args.json, "deliberate", &cast.name, e)),
    }
}

// ---------------------------------------------------------------------------
// generate / cas
// ---------------------------------------------------------------------------

/// Run `kaibo generate` — render through the cast's `image` slot into kaibo's store.
pub async fn run_generate(common: CommonArgs, args: GenerateCliArgs) -> i32 {
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
    match generate_inner(&common, &args, &resolver).await {
        Ok(code) => code,
        Err(SetupError {
            kind,
            message,
            code,
        }) => fail_preflight(args.json, kind, message, code),
    }
}

/// Split one `--field key=value`, reading the value as JSON when it parses as one.
///
/// A provider's wire cares about the difference between `2` and `"2"`, and a shell has
/// only strings — so the value's own JSON syntax carries the type, and anything that is
/// not valid JSON is exactly what it looks like: a string. That keeps `--field n=2` a
/// number and `--field size=1024x1024` a string with no extra flag to learn.
fn parse_generate_field(raw: &str) -> Result<(String, crate::media::FieldValue), SetupError> {
    let (key, value) = raw.split_once('=').ok_or_else(|| SetupError {
        kind: "usage",
        message: format!(
            "`--field {raw}` must be KEY=VALUE, e.g. `--field aspect_ratio=16:9` or \
             `--field n=2`"
        ),
        code: EXIT_USAGE,
    })?;
    if key.is_empty() {
        return Err(SetupError {
            kind: "usage",
            message: format!(
                "`--field {raw}` has an empty key — write `--field KEY=VALUE`, for example \
                 `--field n=2`"
            ),
            code: EXIT_USAGE,
        });
    }
    let typed = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::Bool(b)) => crate::media::FieldValue::Bool(b),
        Ok(serde_json::Value::Number(n)) => crate::media::FieldValue::Num(n),
        // A JSON string, or anything that isn't JSON at all, is a string either way.
        _ => crate::media::FieldValue::Str(value.to_string()),
    };
    Ok((key.to_string(), typed))
}

async fn generate_inner(
    common: &CommonArgs,
    args: &GenerateCliArgs,
    resolver: &Resolver,
) -> Result<i32, SetupError> {
    let cast = resolver
        .resolve_cast(common.cast.clone())
        .map_err(SetupError::usage)?;
    let slot = cast.slot(ModelRole::Image).ok_or_else(|| SetupError {
        kind: "usage",
        message: format!(
            "cast `{}` has no `image` slot — `generate` needs a cast whose `image` slot \
             points at a media backend (kind {}). `kaibo config` lists the configured \
             casts and their slots.",
            cast.name,
            crate::credentials::media_kinds_list(),
        ),
        code: EXIT_USAGE,
    })?;
    let backend = resolver
        .config
        .resolve_backend(&slot.backend)
        .map_err(|e| SetupError {
            kind: "setup",
            message: format!("{e:#}"),
            code: EXIT_SETUP,
        })?;
    // Building the arm resolves the provider key — a missing one is the operator's setup
    // gap, named with the cast and slot.
    let arm = crate::media::MediaArmFactory::build(&crate::media::LiveMediaArms, backend, slot)
        .map_err(|e| SetupError {
            kind: "setup",
            message: format!(
                "`generate` cannot use cast `{}`: its `image` slot did not build — {e:#}",
                cast.name
            ),
            code: EXIT_SETUP,
        })?;
    let mut fields = Vec::with_capacity(args.fields.len());
    for raw in &args.fields {
        fields.push(parse_generate_field(raw)?);
    }
    // The store settles the same way it does on the MCP road, and an artifact with
    // nowhere to land is refused before a provider is paid.
    let persistence = open_batch_store(resolver).await;
    let store = open_media_cas(resolver, persistence.is_some())?.ok_or_else(|| SetupError {
        kind: "usage",
        message: "kaibo's artifact store is disabled (`[cas] enabled = false`), so \
                  `generate` has nowhere to put what it renders. Set `[cas] enabled = true` \
                  in config.toml and run again."
            .to_string(),
        code: EXIT_USAGE,
    })?;

    let request = crate::media::MediaRequest {
        prompt: args.prompt.clone(),
        fields,
        // Text-to-image only from the CLI today: the edit/upscale operations that take an
        // input image would need a file to read, and the CLI has no such argument yet.
        inputs: Vec::new(),
        op: None,
    };
    let outcome = match arm.generate(&request).await {
        Ok(o) => o,
        Err(e) => return Ok(fail_consultation(args.json, "generate", &cast.name, e)),
    };
    // A deferred generation has no `job-N` to hand back here — this process is the job,
    // exactly as `deliberate`'s direct lane is. It polls on the provider's cadence until
    // the call deadline, then says plainly that kaibo stopped watching while the provider
    // may not have stopped working.
    let artifacts = match outcome {
        crate::media::MediaOutcome::Complete { artifacts, note } => {
            // The model's own account of what it did, for a backend whose image
            // generation is a conversation. To stderr, because stdout is the payload —
            // and shown rather than dropped, for the same reason the MCP tool shows it:
            // it is often the only explanation of what came back.
            if let Some(note) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                eprintln!("kaibo: {note}");
            }
            artifacts
        }
        crate::media::MediaOutcome::Deferred(job) => {
            eprintln!(
                "kaibo: the provider is rendering in the background — this process waits \
                 for it (cast `{}`, image `{}`).",
                cast.name,
                arm.slot_ref()
            );
            let deadline = resolver.config.defaults.call_deadline;
            let started = tokio::time::Instant::now();
            loop {
                match arm.poll(&job).await {
                    Ok(crate::media::MediaPollOutcome::Complete(a)) => break a,
                    Ok(crate::media::MediaPollOutcome::Pending) => {
                        if started.elapsed() > deadline {
                            return Ok(fail_consultation(
                                args.json,
                                "generate",
                                &cast.name,
                                anyhow::anyhow!(
                                    "kaibo stopped waiting after {}s, the `[defaults] \
                                     call_deadline` budget. The provider may still finish \
                                     job `{}`, and you may still be billed for it. Raise \
                                     `call_deadline` in config.toml and run again if this \
                                     model needs longer.",
                                    deadline.as_secs(),
                                    job.0
                                ),
                            ));
                        }
                        tokio::time::sleep(crate::server::GENERATE_POLL_INTERVAL).await;
                    }
                    Err(e) => return Ok(fail_consultation(args.json, "generate", &cast.name, e)),
                }
            }
        }
    };
    let text = match crate::server::store_generated_artifacts(
        &store,
        &artifacts,
        &args.prompt,
        arm.slot_ref(),
        &cast.name,
    ) {
        Ok(t) => t,
        Err(e) => return Ok(fail_consultation(args.json, "generate", &cast.name, e)),
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "result": text,
                "cast": cast.name,
                "image": arm.slot_ref(),
            })
        );
    } else {
        println!("{text}");
    }
    Ok(EXIT_OK)
}

/// Run `kaibo cas` — read one artifact by the address you already hold, or store one.
pub async fn run_cas(common: CommonArgs, args: CasArgs) -> i32 {
    init_cli_logging();
    let json = match &args.cmd {
        CasCmd::Read(a) => a.json,
        CasCmd::Write(a) => a.json,
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
    let outcome = match &args.cmd {
        CasCmd::Read(a) => cas_read_inner(a, &resolver).await,
        CasCmd::Write(a) => cas_write_inner(a, &resolver).await,
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

/// Run `kaibo cas write` — store one image file and print its digest.
///
/// **stdout is the payload**, the CLI's standing contract: the bare digest and nothing
/// else, so `DIGEST=$(kaibo cas write shot.png)` works with no flag to remember. Every
/// human-readable detail goes to stderr.
async fn cas_write_inner(args: &CasWriteArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let persistence = open_batch_store(resolver).await;
    let mode = resolver.config.cas_mode(persistence.is_some());
    let store = open_media_cas(resolver, persistence.is_some())?.ok_or_else(|| SetupError {
        kind: "usage",
        message: "kaibo's artifact store is disabled (`[cas] enabled = false`), so there \
                  is nowhere to store an image. Set `[cas] enabled = true` in config.toml \
                  to turn it on."
            .to_string(),
        code: EXIT_USAGE,
    })?;
    if mode == crate::config::CasMode::Memory {
        return Err(SetupError {
            kind: "setup",
            message: "kaibo's artifact store is in-memory this run (persistence is off or \
                      no data directory resolves), and an in-memory store dies with this \
                      process — the digest would be unreadable the moment this command \
                      exits, so nothing was stored. Enable persistence (and a resolvable \
                      $XDG_DATA_HOME or $HOME) to keep artifacts on disk; `kaibo config` \
                      shows the store's mode."
                .to_string(),
            code: EXIT_SETUP,
        });
    }

    // The operator named a file they can already read; kaibo reads it as itself. See the
    // type's doc for why there is no allowed-set check on this path.
    //
    // Stat first, then read. A path named by mistake can be a directory or a 10 GB file,
    // and `std::fs::read` would slurp the whole thing into memory before the cap in
    // `store_bytes` ever refused it — a denial of service by accident is still one. This
    // is the same order `read_contained_file` uses on the MCP side.
    let meta = std::fs::metadata(&args.path).map_err(|e| SetupError {
        kind: "usage",
        message: format!("cannot read {}: {e}", args.path.display()),
        code: EXIT_USAGE,
    })?;
    if !meta.is_file() {
        return Err(SetupError {
            kind: "usage",
            message: format!(
                "{} is not a regular file. Name the image file itself.",
                args.path.display()
            ),
            code: EXIT_USAGE,
        });
    }
    if meta.len() > crate::upload::MAX_UPLOAD_BYTES as u64 {
        return Err(SetupError {
            kind: "usage",
            message: format!(
                "{} is {} bytes, over the {}-byte cap. Nothing was read or stored. Reduce \
                 the image's resolution or re-encode it at a lower quality, then store the \
                 smaller file.",
                args.path.display(),
                meta.len(),
                crate::upload::MAX_UPLOAD_BYTES,
            ),
            code: EXIT_USAGE,
        });
    }
    let bytes = std::fs::read(&args.path).map_err(|e| SetupError {
        kind: "usage",
        message: format!("cannot read {}: {e}", args.path.display()),
        code: EXIT_USAGE,
    })?;

    let stored = crate::upload::store_bytes(
        &store,
        &bytes,
        args.label.as_deref(),
        crate::server::render::now_epoch_secs(),
    )
    // A store I/O failure or capacity refusal is an environmental condition, not an
    // argument the caller got wrong — the same split the MCP tool makes between
    // `invalid_params` and `internal_error`, and the one `cas_read_inner` already makes
    // for a failed `store.get`.
    .map_err(|e| match e {
        crate::upload::UploadError::Store(_) => SetupError {
            kind: "setup",
            message: e.to_string(),
            code: EXIT_SETUP,
        },
        _ => SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        },
    })?;

    let hex = stored.digest.to_hex();
    let path = store.path_for(&stored.digest);
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "digest": hex,
                "uri": format!("{}{hex}", crate::cas::CAS_URI_PREFIX),
                "mime": stored.extension.mime(),
                "bytes": stored.bytes,
                "path": path.as_ref().map(|p| p.display().to_string()),
                "provenance_recorded": !stored.provenance_missing,
            })
        );
        return Ok(EXIT_OK);
    }

    println!("{hex}");
    eprintln!(
        "stored {} ({}, {} bytes) as {}{hex}",
        args.path.display(),
        stored.extension.mime(),
        stored.bytes,
        crate::cas::CAS_URI_PREFIX,
    );
    if let Some(p) = path {
        eprintln!("  path: {}", p.display());
    }
    if stored.provenance_missing {
        eprintln!(
            "  NOTE: the image is stored and readable, but kaibo could not record the \
             metadata beside it — nothing on disk says what this image is or when it \
             arrived. The digest above is still good."
        );
    }
    Ok(EXIT_OK)
}

async fn cas_read_inner(args: &CasReadArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let persistence = open_batch_store(resolver).await;
    let mode = resolver.config.cas_mode(persistence.is_some());
    let store = open_media_cas(resolver, persistence.is_some())?.ok_or_else(|| SetupError {
        kind: "usage",
        message: "kaibo's artifact store is disabled (`[cas] enabled = false`), so kaibo \
                  holds no artifacts to read. Set `[cas] enabled = true` in config.toml to \
                  turn it on."
            .to_string(),
        code: EXIT_USAGE,
    })?;
    // Memory mode is worth saying out loud on THIS command: a fresh process gets a fresh
    // empty store, so no digest can ever resolve. Reporting "not found" alone would send
    // someone hunting for a typo in an address that was never the problem.
    if mode == crate::config::CasMode::Memory {
        return Err(SetupError {
            kind: "setup",
            message: "kaibo's artifact store is in-memory this run (persistence is off or \
                      no data directory resolves), and an in-memory store is empty in a \
                      new process — nothing is readable by digest. Enable persistence \
                      (and a resolvable $XDG_DATA_HOME or $HOME) to keep artifacts on \
                      disk; `kaibo config` shows the store's mode."
                .to_string(),
            code: EXIT_SETUP,
        });
    }
    // Either spelling kaibo prints is accepted — refusing the exact string we handed back
    // would be a puzzle, not a guardrail.
    let hex = args
        .digest
        .trim()
        .strip_prefix(crate::cas::CAS_URI_PREFIX)
        .unwrap_or(args.digest.trim());
    let digest = crate::cas::Digest::from_hex(hex).map_err(|e| SetupError {
        kind: "usage",
        message: format!(
            "{e} — pass the digest kaibo printed, either bare or as its full {}<digest> URI.",
            crate::cas::CAS_URI_PREFIX
        ),
        code: EXIT_USAGE,
    })?;
    // The store verifies content against its address before a byte comes back, so a
    // corrupt object is loud here rather than silently served.
    let (bytes, ext) = match store.get(&digest) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(SetupError {
                kind: "usage",
                message: format!(
                    "no artifact with digest {hex} in kaibo's artifact store. Pass a digest \
                     kaibo printed — a `generate` result, or one from a \
                     `kaibo://cas/<digest>` footer. `kaibo config` shows which store \
                     directory this process reads."
                ),
                code: EXIT_USAGE,
            })
        }
        Err(e) => {
            return Err(SetupError {
                kind: "setup",
                message: format!("{e:#}"),
                code: EXIT_SETUP,
            })
        }
    };

    let provenance = store.provenance(&digest);
    let path = store.path_for(&digest);
    let obj = crate::server::CasObject {
        digest: hex,
        ext,
        bytes: &bytes,
        label: provenance.as_ref().and_then(|p| p.label.as_deref()),
        provenance_present: provenance.is_some(),
        path: path.as_deref(),
    };
    let view = crate::server::plan_cas_read(
        &obj,
        args.offset,
        args.length,
        crate::server::Delivery::Bytes,
    )
    .map_err(|message| SetupError {
        kind: "usage",
        message,
        code: EXIT_USAGE,
    })?;

    // `--json` keeps the MCP shape: one object on stdout, a binary body base64-encoded
    // because JSON has no other way to carry bytes.
    if args.json {
        let body = match &view.body {
            crate::server::CasBody::None => None,
            crate::server::CasBody::Text(t) => Some(t.clone()),
            crate::server::CasBody::Base64(b) => Some(b.clone()),
            crate::server::CasBody::Image { data, .. } => Some(data.clone()),
        };
        println!(
            "{}",
            serde_json::json!({
                "metadata": view.meta,
                "body": body,
                "binary": !ext.is_textual(),
            })
        );
        return Ok(EXIT_OK);
    }

    // Prose form follows the CLI's standing contract — **stdout is the payload, stderr is
    // everything else** — which is what makes `kaibo cas read <digest> > arch.png` work
    // with no flag to remember. The metadata a `read_cas` caller needs in-band goes to
    // stderr here, because a terminal caller reads it with their eyes and a script must
    // not have to strip it.
    //
    // The rules that bound an MCP read exist to protect a CONTEXT WINDOW. A pipe has no
    // such budget, and someone who asked for a PNG by its digest knows what is coming — so
    // a binary object's content goes out as bytes, never as base64 nobody wants. That is
    // the one place this front door legitimately differs from the tool.
    eprintln!("{}", view.meta);
    match view.body {
        crate::server::CasBody::None => {}
        crate::server::CasBody::Text(t) => println!("{t}"),
        // Binary: the exact bytes the metadata just described, straight to stdout. This is
        // the single blessed write site outside the two stores — see the marker, and
        // `tests/no_write_path.rs`, whose pinned count makes it impossible to add another
        // one quietly. Nothing is created on any filesystem here: stdout is a descriptor
        // the operator's shell already opened and aimed, so kaibo still has no
        // destination parameter anywhere in this store's surface.
        crate::server::CasBody::Base64(_) | crate::server::CasBody::Image { .. } => {
            let start = args.offset.min(bytes.len());
            let end = match args.length {
                Some(n) => start.saturating_add(n).min(bytes.len()),
                None => bytes.len(),
            };
            use std::io::Write as _;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            out.write_all(&bytes[start..end]) // cas-stdout-bytes: blessed by the media CAS invariant amendment (AGENTS.md)
                .and_then(|()| out.flush())
                .map_err(|e| SetupError {
                    kind: "setup",
                    message: format!(
                        "kaibo could not write the artifact to stdout, so the output is \
                 incomplete: {e}. Redirect to a file and run again: \
                 `kaibo cas read <digest> > out.png`."
                    ),
                    code: EXIT_SETUP,
                })?;
        }
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------
// kaish
// ---------------------------------------------------------------------------

/// Run `kaibo kaish -c 'SCRIPT'` — one non-interactive execution through the read-only
/// sandbox. stdout carries the script's stdout, stderr its stderr, and the process exits
/// with kaish's own exit code (0 ok, 1 the command failed, 124 timed out, 127 command
/// not found). A missing `-c` is a
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
            eprintln!(
                "kaibo: could not start the kaish shell, so nothing ran: {e:#}. Check that \
                 the project path is a readable directory — `kaibo config` prints the \
                 allowed trees and the default root."
            );
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
            eprintln!(
                "kaibo: the kaish shell failed while running your script, so its output is \
                 incomplete: {e:#}. This is a kaibo failure, not a script error — a script's \
                 own exit codes are 0 (ok), 1 (the command failed, which is also how a \
                 refused write reports), 124 (timed out), and 127 (command not found)."
            );
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
        if !batch_supported(b) {
            return Err(SetupError {
                kind: "usage",
                message: format!(
                    "backend `{}` (kind `{}`) has no batch lane, so kaibo cannot list its \
                     batches. Batch-capable kinds: {}. Omit --backend to list every \
                     batch-capable backend.",
                    b.name,
                    b.kind.canonical_name(),
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
        .filter(|b| batch_supported(b))
        .map(|b| b.name.clone())
        .collect();
    if names.is_empty() {
        return Err(SetupError {
            kind: "setup",
            message: format!(
                "no configured backend has a batch lane, so there is nothing to list. \
                 Batch-capable kinds: {}. Add a backend of one of those kinds — \
                 `kaibo example-config` prints an annotated template, and `kaibo config` \
                 shows what kaibo resolved.",
                supported_kinds_list()
            )
            .to_string(),
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

/// Split a `backend/provider-id` handle, or refuse it by name.
///
/// The split is unambiguous because a backend name carries no `/` (enforced at config
/// load), so the FIRST `/` is always the boundary even when the provider id contains more
/// (a Gemini id is `batches/<id>`). Shared by every verb that takes a handle.
fn split_batch_handle(handle: &str) -> Result<(&str, &str), SetupError> {
    handle
        .split_once('/')
        .filter(|(b, id)| !b.is_empty() && !id.is_empty())
        .ok_or_else(|| SetupError {
            kind: "usage",
            message: format!(
                "batch handle {handle:?} must be \"backend/provider-id\" — pass the handle \
                 `kaibo batch submit` or `kaibo deliberate` printed"
            ),
            code: EXIT_USAGE,
        })
}

async fn batch_get_inner(args: &BatchGetArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let (backend_name, provider_id) = split_batch_handle(&args.handle)?;
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
    // The submitted count this handle was recorded with, when persistence is on and the
    // record has one — `None` (no cross-check to run) for persistence off, an unrecorded
    // handle, or a lookup error; the provider stays the source of truth for the poll
    // itself regardless.
    let submitted = match open_batch_store(resolver).await {
        Some(store) => match store.get_batch(backend_name, provider_id).await {
            Ok(handle) => handle.and_then(|h| h.submitted_count).map(|n| n as u64),
            Err(e) => {
                tracing::warn!(handle = %args.handle, error = %e, "could not read the batch handle's submitted count");
                None
            }
        },
        None => None,
    };
    match provider.poll(provider_id, submitted).await {
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

/// Ask the provider to stop a batch.
///
/// Cancellation is a REQUEST, not a guarantee: a provider finishes what is already in
/// flight, so the honest word to a caller is "requested" and the way to learn what
/// actually happened is `kaibo batch get` (whose poll reports the terminal state and any
/// items that landed anyway). kaibo deletes no record here — a canceled batch's finished
/// items are still yours, and still cost what they cost.
async fn batch_cancel_inner(args: &BatchGetArgs, resolver: &Resolver) -> Result<i32, SetupError> {
    let (backend_name, provider_id) = split_batch_handle(&args.handle)?;
    let backend = resolver
        .config
        .resolve_backend(backend_name)
        .map_err(|e| SetupError {
            kind: "usage",
            message: e.to_string(),
            code: EXIT_USAGE,
        })?;
    // A backend with no batch lane at all is the caller naming the wrong thing, not a
    // broken setup — say which kinds have one rather than failing later, deeper, and less
    // clearly on the poller construction (DeepSeek cross-family review, 2026-08-07).
    if !crate::batch::batch_supported(backend) {
        return Err(SetupError {
            kind: "usage",
            message: format!(
                "backend `{}` (kind `{}`) has no batch lane, so it holds no batch to \
                 cancel. Batch-capable kinds: {}.",
                backend.name,
                backend.kind.canonical_name(),
                crate::batch::supported_kinds_list()
            ),
            code: EXIT_USAGE,
        });
    }
    // Poll and cancel need only the connection, never a cast or a model — which is why a
    // handle stays addressable from any front door, after any restart.
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
                    serde_json::json!({
                        "handle": args.handle,
                        "status": "cancel_requested",
                    })
                );
            } else {
                eprintln!(
                    "kaibo: requested cancellation of `{}` — `kaibo batch get {}` for the \
                     final state and any items that finished first.",
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
    // Recovered handles from the store, deduped against the live listing.
    let mut recovered: Vec<(String, Option<String>)> = Vec::new();
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
                "\nRecovered batch handles (submitted by kaibo, read from the state db — \
                 run `kaibo batch get <handle>` for live status):\n{}",
                lines.join("\n")
            );
        }
    }
    Ok(EXIT_OK)
}

/// Run `kaibo models` — read-only model discovery, no cast/model in the loop. Returns
/// the process exit code: 2/3 for a pre-flight config/setup rejection, 4 only when
/// *every* queried backend's discovery call failed (a per-backend error inside an
/// otherwise-successful sweep is reported inline and is NOT fatal — matching `batch
/// list`'s inline-error convention), else 0.
pub async fn run_models(common: CommonArgs, args: ModelsArgs) -> i32 {
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

    // An explicit --backend scopes to that one name/alias (resolved to its canonical
    // name so the results map keys consistently); omitted sweeps every configured
    // backend, sorted (BTreeMap order) same as `batch list`.
    let names: Vec<String> = match args.backend.as_deref() {
        Some(name) => match resolver.config.resolve_backend(name) {
            Ok(b) => vec![b.name.clone()],
            Err(e) => return fail_preflight(args.json, "usage", e.to_string(), EXIT_USAGE),
        },
        None => resolver.config.backends.keys().cloned().collect(),
    };
    if names.is_empty() {
        return fail_preflight(
            args.json,
            "setup",
            "no backend is configured, so there is nothing to query. Add a \
             `[backends.<name>]` section to config.toml — `kaibo example-config` prints an \
             annotated template, and `kaibo config` shows the resolved configuration."
                .to_string(),
            EXIT_SETUP,
        );
    }

    let mut results: BTreeMap<String, Result<Vec<crate::discover::DiscoveredModel>, String>> =
        BTreeMap::new();
    for name in &names {
        // Each name above already resolved once; re-resolving here is cheap and
        // avoids holding a borrow of `resolver.config` across the `.await` below.
        let Ok(backend) = resolver.config.resolve_backend(name) else {
            continue;
        };
        let fetcher = crate::discover::HttpModelListFetcher::new(backend.request_timeout);
        let outcome = crate::discover::discover_models(backend, &fetcher)
            .await
            .map_err(|e| format!("{e:#}"));
        results.insert(name.clone(), outcome);
    }

    if args.json {
        println!("{}", crate::discover::models_json_envelope(&results));
    } else {
        println!("{}", crate::discover::render_models(&results));
    }

    if results.values().all(Result::is_err) {
        EXIT_CONSULT_FAILURE
    } else {
        EXIT_OK
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

    /// `kaibo --help` (long form) documents the exit-code taxonomy — the only place a
    /// script author reading `--help` cold learns it. `-h` (short form) stays terse.
    #[test]
    fn long_help_documents_exit_codes() {
        let long_help = Cli::command().render_long_help().to_string();
        assert!(
            long_help.contains("EXIT CODES"),
            "long --help should carry the exit-code table:\n{long_help}"
        );
        for code in [
            // Row 0 says "success", not "an answer": `config`, `models`, `example-config`,
            // and `cas read` all exit 0 without producing an answer.
            "0  success",
            "2  usage error",
            "3  setup/containment",
            "4  the work ran",
        ] {
            assert!(
                long_help.contains(code),
                "long --help should document exit code {code:?}:\n{long_help}"
            );
        }
        let short_help = Cli::command().render_help().to_string();
        assert!(
            !short_help.contains("EXIT CODES"),
            "short -h should stay terse, not carry the full exit-code table"
        );
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
    fn configure_subcommand_parses_with_and_without_goal() {
        let cli = Cli::try_parse_from(["kaibo", "configure"]).expect("configure parse");
        match cli.command {
            Some(Command::Configure(c)) => assert_eq!(c.goal, None),
            other => panic!("expected configure, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["kaibo", "configure", "a local-only privacy cast"])
            .expect("configure parse with goal");
        match cli.command {
            Some(Command::Configure(c)) => {
                assert_eq!(c.goal.as_deref(), Some("a local-only privacy cast"))
            }
            other => panic!("expected configure, got {other:?}"),
        }
    }

    #[test]
    fn example_config_subcommand_parses() {
        let cli = Cli::try_parse_from(["kaibo", "example-config"]).expect("example-config parse");
        assert!(matches!(cli.command, Some(Command::ExampleConfig)));
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

    /// `--field` carries a TYPE, because a provider's wire distinguishes `2` from `"2"`
    /// and a shell has only strings. The value's own JSON syntax is what decides, so
    /// nothing extra has to be learned — and a value that is not JSON at all is exactly
    /// what it looks like.
    #[test]
    fn a_generate_field_takes_its_type_from_the_value() {
        use crate::media::FieldValue;

        let (k, v) = parse_generate_field("n=2").expect("a number");
        assert_eq!(k, "n");
        assert!(
            matches!(&v, FieldValue::Num(n) if n.to_string() == "2"),
            "an integer must stay integral, got {v:?}"
        );
        assert!(matches!(
            parse_generate_field("transparent=true").unwrap().1,
            FieldValue::Bool(true)
        ));
        // Not JSON, and not a mistake: the common case for an image knob.
        assert!(matches!(
            parse_generate_field("size=1024x1024").unwrap().1,
            FieldValue::Str(s) if s == "1024x1024"
        ));
        // A quoted JSON string keeps its content, not its quotes.
        assert!(matches!(
            parse_generate_field("style=\"3d-model\"").unwrap().1,
            FieldValue::Str(s) if s == "\"3d-model\""
        ));
        // The value may itself contain `=` — only the FIRST one splits.
        let (k, v) = parse_generate_field("prompt_suffix=a=b").expect("split on the first =");
        assert_eq!(k, "prompt_suffix");
        assert!(matches!(v, FieldValue::Str(s) if s == "a=b"));

        for bad in ["noequals", "=value"] {
            let err = parse_generate_field(bad).expect_err("must be refused");
            assert_eq!(err.code, EXIT_USAGE);
        }
    }

    /// A cast with no `image` slot cannot generate, and the refusal says what a cast needs
    /// rather than failing later in a provider call. Exit 2, before any network.
    #[tokio::test]
    async fn generate_on_a_cast_without_an_image_slot_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let cli = Cli::parse_from(["kaibo", "generate", "a cat", "--cast", "deepseek"]);
        let common = cli.common.clone();
        let args = match cli.command {
            Some(Command::Generate(g)) => g,
            other => panic!("expected generate, got {other:?}"),
        };
        let err = generate_inner(&common, &args, &resolver)
            .await
            .expect_err("a cast with no image slot must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
        assert!(
            err.message.contains("`image` slot"),
            "the refusal names what the cast is missing: {}",
            err.message
        );
    }

    /// A real read, end to end: a disk-mode store, an object put in it, and the digest
    /// read back through the command — the path that reaches `plan` and then writes to
    /// stdout. Everything else here stops at a refusal, so without this the byte-serving
    /// half of the command has no test at all (DeepSeek cross-family review, 2026-08-07).
    #[tokio::test]
    async fn cas_read_serves_a_stored_object_by_its_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().join("proj"));
        std::fs::create_dir_all(dir.path().join("proj")).unwrap();
        // Disk mode needs persistence active AND a resolvable cas dir, both outside the
        // allowed tree — the same containment rule the store enforces in production.
        config.persistence.enabled = true;
        config.persistence.path = Some(dir.path().join("state.db"));
        config.cas.dir = Some(dir.path().join("cas"));
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        // Put an object the way a producer would, through the store's own write path.
        let cas = crate::cas::Cas::open(&dir.path().join("cas"), &[], None).expect("cas opens");
        let digest = cas
            .put(
                b"the artifact body",
                crate::cas::Extension::Txt,
                &crate::cas::Provenance {
                    prompt: "p".into(),
                    model: "m".into(),
                    cast: "c".into(),
                    timestamp: 0,
                    mime: "text/plain".into(),
                    seed: None,
                    tool: Some("generate".into()),
                    slot: None,
                    label: Some("a stored artifact".into()),
                    session: None,
                },
            )
            .expect("the object stores");

        // Both spellings kaibo prints resolve to the same object.
        for reference in [
            digest.to_hex(),
            format!("{}{}", crate::cas::CAS_URI_PREFIX, digest.to_hex()),
        ] {
            let cli = Cli::parse_from(["kaibo", "cas", "read", &reference]);
            let args = match cli.command {
                Some(Command::Cas(c)) => match c.cmd {
                    CasCmd::Read(r) => r,
                    other => panic!("expected cas read, got {other:?}"),
                },
                other => panic!("expected cas read, got {other:?}"),
            };
            let code = cas_read_inner(&args, &resolver)
                .await
                .unwrap_or_else(|e| panic!("reading {reference} must succeed: {}", e.message));
            assert_eq!(code, EXIT_OK);
        }

        // A digest this store never held is a usage error naming the digest, not a crash.
        let cli = Cli::parse_from(["kaibo", "cas", "read", &"bb".repeat(32)]);
        let args = match cli.command {
            Some(Command::Cas(c)) => match c.cmd {
                CasCmd::Read(r) => r,
                other => panic!("expected cas read, got {other:?}"),
            },
            other => panic!("expected cas read, got {other:?}"),
        };
        let err = cas_read_inner(&args, &resolver)
            .await
            .expect_err("an absent digest is refused");
        assert_eq!(err.code, EXIT_USAGE);
    }

    /// A minimal but real PNG header — a true signature, so `sniff_image` is not being
    /// fed a lie the way a fixture of arbitrary bytes would be.
    #[cfg(test)]
    fn png_file_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        v
    }

    /// Build a disk-mode resolver plus its temp dir, the shape both `cas` verbs need.
    #[cfg(test)]
    fn disk_cas_resolver() -> (Resolver, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().join("proj"));
        std::fs::create_dir_all(dir.path().join("proj")).unwrap();
        config.persistence.enabled = true;
        config.persistence.path = Some(dir.path().join("state.db"));
        config.cas.dir = Some(dir.path().join("cas"));
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();
        (resolver, dir)
    }

    #[cfg(test)]
    fn parse_cas_write(argv: &[&str]) -> CasWriteArgs {
        match Cli::parse_from(argv).command {
            Some(Command::Cas(c)) => match c.cmd {
                CasCmd::Write(w) => w,
                other => panic!("expected cas write, got {other:?}"),
            },
            other => panic!("expected cas write, got {other:?}"),
        }
    }

    /// **`cas write` then `cas read` — the operator's round trip.**
    ///
    /// The gap this command closes: `read_cas`/`kaibo cas read` could always get an
    /// artifact out, and until now only an MCP client could put one in. This asserts a
    /// file written from the command line is readable back by the digest the command
    /// printed.
    #[tokio::test]
    async fn cas_write_stores_a_file_and_cas_read_gets_it_back() {
        let (resolver, dir) = disk_cas_resolver();
        let img = dir.path().join("screenshot.png");
        std::fs::write(&img, png_file_bytes()).unwrap();

        let args = parse_cas_write(&[
            "kaibo",
            "cas",
            "write",
            img.to_str().unwrap(),
            "--label",
            "the failing dialog",
        ]);
        assert_eq!(args.label.as_deref(), Some("the failing dialog"));
        let code = cas_write_inner(&args, &resolver)
            .await
            .unwrap_or_else(|e| panic!("writing must succeed: {}", e.message));
        assert_eq!(code, EXIT_OK);

        // The digest the command computed is the content's own hash, so recompute it
        // rather than scraping stdout, and prove `cas read` resolves it.
        let digest = crate::cas::Digest::of_bytes(&png_file_bytes());
        // `--json` on the read leg: `cas read` without it writes the object's raw bytes to
        // stdout, which is its documented contract and exactly wrong inside a test suite —
        // NUL bytes in the captured log make `grep` treat the whole thing as binary and
        // silently print nothing, which is how a green run can look like no run at all.
        let read = parse_cas_read(&["kaibo", "cas", "read", &digest.to_hex(), "--json"]);
        let code = cas_read_inner(&read, &resolver)
            .await
            .unwrap_or_else(|e| panic!("the digest just written must read: {}", e.message));
        assert_eq!(code, EXIT_OK);
    }

    #[cfg(test)]
    fn parse_cas_read(argv: &[&str]) -> CasReadArgs {
        match Cli::parse_from(argv).command {
            Some(Command::Cas(c)) => match c.cmd {
                CasCmd::Read(r) => r,
                other => panic!("expected cas read, got {other:?}"),
            },
            other => panic!("expected cas read, got {other:?}"),
        }
    }

    /// The admission rules are the store's, not the command's — a text file is refused
    /// here exactly as it is through the MCP tool, and the refusal names the formats.
    #[tokio::test]
    async fn cas_write_refuses_a_file_that_is_not_an_image() {
        let (resolver, dir) = disk_cas_resolver();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, b"# not an image\n").unwrap();

        let err = cas_write_inner(
            &parse_cas_write(&["kaibo", "cas", "write", notes.to_str().unwrap()]),
            &resolver,
        )
        .await
        .expect_err("a markdown file is not an image");
        assert_eq!(err.code, EXIT_USAGE);
        assert!(err.message.contains("png"), "{}", err.message);
    }

    /// A missing file is a usage error naming the path, not a panic and not a silent
    /// zero-byte store.
    #[tokio::test]
    async fn cas_write_names_the_file_it_could_not_read() {
        let (resolver, dir) = disk_cas_resolver();
        let missing = dir.path().join("nope.png");
        let err = cas_write_inner(
            &parse_cas_write(&["kaibo", "cas", "write", missing.to_str().unwrap()]),
            &resolver,
        )
        .await
        .expect_err("no such file");
        assert_eq!(err.code, EXIT_USAGE);
        assert!(err.message.contains("nope.png"), "{}", err.message);
    }

    /// **The cap is enforced from the file's metadata, before a byte is read.**
    ///
    /// `store_bytes` would refuse an over-cap image anyway, but only after `std::fs::read`
    /// had already pulled the whole thing into memory — so a path named by mistake (a disk
    /// image, a core dump) would be a denial of service by accident before it was a
    /// refusal. This drives a file one byte over the cap and asserts the refusal names the
    /// size, which it can only do from the stat.
    #[tokio::test]
    async fn cas_write_refuses_an_over_cap_file_from_its_metadata() {
        let (resolver, dir) = disk_cas_resolver();
        // Sparse: `set_len` makes the file huge by *metadata* while allocating nothing.
        // That is exactly what the check under test reads, and writing 8 MiB of real
        // bytes into a tmpfs-backed temp dir alongside the rest of the suite is how you
        // get a SIGBUS in an unrelated test that mmaps its store.
        let big = dir.path().join("huge.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(crate::upload::MAX_UPLOAD_BYTES as u64 + 1)
            .unwrap();
        drop(f);

        let err = cas_write_inner(
            &parse_cas_write(&["kaibo", "cas", "write", big.to_str().unwrap()]),
            &resolver,
        )
        .await
        .expect_err("over the cap");
        assert_eq!(err.code, EXIT_USAGE);
        assert!(
            err.message.contains("over the")
                && err
                    .message
                    .contains(&crate::upload::MAX_UPLOAD_BYTES.to_string()),
            "the refusal names the cap: {}",
            err.message
        );
        assert!(
            err.message.contains("Nothing was read or stored"),
            "and says the bytes were never read: {}",
            err.message
        );
    }

    /// A directory produces an OS-dependent error from `std::fs::read` ("is a directory"
    /// on Unix, something else elsewhere). Checking `is_file()` first gives one clear
    /// message on every platform, the same one the MCP side gives.
    #[tokio::test]
    async fn cas_write_refuses_a_directory_with_one_clear_message() {
        let (resolver, dir) = disk_cas_resolver();
        let sub = dir.path().join("pictures");
        std::fs::create_dir_all(&sub).unwrap();

        let err = cas_write_inner(
            &parse_cas_write(&["kaibo", "cas", "write", sub.to_str().unwrap()]),
            &resolver,
        )
        .await
        .expect_err("a directory is not an image file");
        assert_eq!(err.code, EXIT_USAGE);
        assert!(
            err.message.contains("not a regular file"),
            "{}",
            err.message
        );
    }

    /// **An in-memory store is refused, not quietly written to.**
    ///
    /// A one-shot CLI process storing into a store that dies with it has done nothing —
    /// the digest it printed could never resolve again. Reporting success there would
    /// hand back an address that is already dead, which is worse than refusing.
    #[tokio::test]
    async fn cas_write_refuses_an_in_memory_store_rather_than_printing_a_dead_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().join("proj"));
        std::fs::create_dir_all(dir.path().join("proj")).unwrap();
        // Persistence off ⇒ memory mode, whatever the cas dir says.
        config.persistence.enabled = false;
        config.cas.dir = Some(dir.path().join("cas"));
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let img = dir.path().join("shot.png");
        std::fs::write(&img, png_file_bytes()).unwrap();
        let err = cas_write_inner(
            &parse_cas_write(&["kaibo", "cas", "write", img.to_str().unwrap()]),
            &resolver,
        )
        .await
        .expect_err("an in-memory store cannot hold a CLI write");
        assert_eq!(err.code, EXIT_SETUP);
        assert!(err.message.contains("in-memory"), "{}", err.message);
    }

    /// A memory-mode store is empty in a new process, so `kaibo cas read` refuses and
    /// names the mode. Reporting "not found" would send a caller hunting for a typo in an
    /// address that was never the problem.
    #[tokio::test]
    async fn cas_read_in_memory_mode_says_so_instead_of_reporting_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        // Persistence off ⇒ the store is in-memory ⇒ nothing is readable. That refusal
        // comes first and says so plainly, which is the honest answer: hunting for a typo
        // in a digest that was never the problem is the failure mode to avoid.
        config.persistence.enabled = false;
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let cli = Cli::parse_from(["kaibo", "cas", "read", "../../etc/passwd"]);
        let args = match cli.command {
            Some(Command::Cas(c)) => match c.cmd {
                CasCmd::Read(r) => r,
                other => panic!("expected cas read, got {other:?}"),
            },
            other => panic!("expected cas read, got {other:?}"),
        };
        let err = cas_read_inner(&args, &resolver)
            .await
            .expect_err("an in-memory store holds nothing to read");
        assert!(
            err.message.contains("in-memory"),
            "a memory-mode read says why nothing can be found: {}",
            err.message
        );
        assert_eq!(err.code, EXIT_SETUP);
    }

    /// The `cas` group carries exactly one verb, and it parses its window flags.
    #[test]
    fn cas_read_parses_its_window() {
        let cli = Cli::parse_from([
            "kaibo",
            "cas",
            "read",
            "kaibo://cas/abc",
            "--offset",
            "64",
            "--length",
            "128",
            "--json",
        ]);
        match cli.command {
            Some(Command::Cas(c)) => match c.cmd {
                CasCmd::Read(r) => {
                    assert_eq!(r.digest, "kaibo://cas/abc");
                    assert_eq!(r.offset, 64);
                    assert_eq!(r.length, Some(128));
                    assert!(r.json);
                }
                other => panic!("expected cas read, got {other:?}"),
            },
            other => panic!("expected cas read, got {other:?}"),
        }
    }

    /// `kaibo deliberate` takes the same arguments as the MCP tool under the CLI's
    /// spellings, and the global flags still land on `common`.
    #[test]
    fn deliberate_subcommand_parses_its_flags() {
        let cli = Cli::parse_from([
            "kaibo",
            "deliberate",
            "is this abstraction right?",
            "--cast",
            "gemini-deliberate",
            "--path",
            "/tmp/proj",
            "--attach",
            "src/a.rs",
            "--attach",
            "src/b.rs",
            "--synth-model",
            "other-synth",
            "--explorer-max-turns",
            "7",
            "--json",
        ]);
        assert_eq!(cli.common.cast.as_deref(), Some("gemini-deliberate"));
        match cli.command {
            Some(Command::Deliberate(d)) => {
                assert_eq!(d.question, "is this abstraction right?");
                assert_eq!(d.path.as_deref(), Some("/tmp/proj"));
                assert_eq!(d.attach, vec!["src/a.rs", "src/b.rs"]);
                assert_eq!(d.synth_model.as_deref(), Some("other-synth"));
                assert_eq!(d.explorer_max_turns, Some(7));
                assert!(d.json);
                assert_eq!(d.dossier, None);
            }
            other => panic!("expected deliberate, got {other:?}"),
        }
    }

    /// The reuse road on the CLI: the explorer flags `--dossier` makes inert are refused
    /// **by name**, before anything resolves — never stripped, which would run a call the
    /// caller did not ask for and hand back an answer that looks like it honored them.
    #[tokio::test]
    async fn deliberate_with_a_dossier_and_attach_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let digest = "aa".repeat(32);
        let cli = Cli::parse_from([
            "kaibo",
            "deliberate",
            "q",
            "--dossier",
            &digest,
            "--attach",
            "src/x.rs",
        ]);
        let common = cli.common.clone();
        let args = match cli.command {
            Some(Command::Deliberate(d)) => d,
            other => panic!("expected deliberate, got {other:?}"),
        };
        let err = deliberate_inner(&common, &args, &resolver)
            .await
            .expect_err("`--attach` with `--dossier` must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
        assert!(
            err.message.contains("`attach`"),
            "the refusal names the inert flag: {}",
            err.message
        );
    }

    /// A cast with no OFFLINE synth cannot deliberate; the refusal names casts that can
    /// and points at `consult` for an answer this turn. Exit 2, before any network.
    #[tokio::test]
    async fn deliberate_on_an_interactive_cast_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        // `anthropic` is interactive — its synth sits on no offline lane.
        let cli = Cli::parse_from(["kaibo", "deliberate", "q", "--cast", "anthropic"]);
        let common = cli.common.clone();
        let args = match cli.command {
            Some(Command::Deliberate(d)) => d,
            other => panic!("expected deliberate, got {other:?}"),
        };
        let err = deliberate_inner(&common, &args, &resolver)
            .await
            .expect_err("an interactive cast on deliberate must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
        assert!(
            err.message.contains("no offline synth") && err.message.contains("kaibo consult"),
            "the refusal explains the lane and names the alternative: {}",
            err.message
        );
    }

    /// `batch cancel` takes a handle, and the shared split every handle verb uses keeps
    /// `get` and `cancel` from drifting apart. The FIRST `/` is the boundary, so a Gemini
    /// id keeps its own slashes.
    #[test]
    fn batch_cancel_parses_a_handle_and_refuses_a_malformed_one() {
        let cli = Cli::parse_from(["kaibo", "batch", "cancel", "gem/batches/abc"]);
        match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Cancel(c) => assert_eq!(c.handle, "gem/batches/abc"),
                other => panic!("expected cancel, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        }

        let (backend, id) = split_batch_handle("gem/batches/abc").expect("a valid handle splits");
        assert_eq!((backend, id), ("gem", "batches/abc"));

        for bad in ["no-slash", "/leading", "trailing/"] {
            let err = split_batch_handle(bad).expect_err("a malformed handle is refused");
            assert_eq!(err.code, EXIT_USAGE);
            assert!(
                err.message.contains("backend/provider-id"),
                "the refusal shows the shape: {}",
                err.message
            );
        }
    }

    /// A cast that can actually deliberate, for the tests that need one — no built-in
    /// pairs an explorer with an offline synth (see `tests/gating.rs`), so a fixture is
    /// the only way to reach the road past the lane check. `key_optional` keeps it
    /// resolvable with no credential in the environment.
    const DELIBERATE_CAST_TOML: &str = r#"
        [backends.gem]
        kind = "gemini"
        key_optional = true

        [casts.mydeliberate]
        explorer = "gem/some-lite"
        synth    = { backend = "gem", id = "some-pro", lane = "batch" }
    "#;

    /// **The lane must be captured BEFORE the model overrides run.** `apply_model_override`
    /// replaces a slot with a *laneless* one, so reading `synth_lane()` afterward sees
    /// `None` and refuses every deliberate-capable cast as "no offline synth" — the
    /// headline feature broken for everyone by a two-line reordering. The MCP road pins
    /// this in `deliberation_lane_with_overrides`; the CLI inlines the same order and had
    /// nothing holding it (DeepSeek cross-family review, 2026-08-07).
    ///
    /// Driving it: a deliberate cast WITH a synth override must get past the lane check.
    /// It fails further along on this keyless fixture, which is fine — any outcome except
    /// the lane refusal proves the capture survived the override.
    #[tokio::test]
    async fn the_deliberate_lane_survives_a_synth_override_on_the_cli() {
        let config = Config::from_toml_str(DELIBERATE_CAST_TOML).expect("fixture config parses");
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let cli = Cli::parse_from([
            "kaibo",
            "deliberate",
            "q",
            "--cast",
            "mydeliberate",
            "--synth-model",
            "some-other-pro",
        ]);
        let common = cli.common.clone();
        let args = match cli.command {
            Some(Command::Deliberate(d)) => d,
            other => panic!("expected deliberate, got {other:?}"),
        };
        if let Err(err) = deliberate_inner(&common, &args, &resolver).await {
            assert!(
                !err.message.contains("no offline synth"),
                "the lane must be captured before the synth override runs, but the call \
                 was refused as laneless: {}",
                err.message
            );
        }
    }

    /// A cancel naming a backend with no batch lane is refused as a USAGE error naming the
    /// batch-capable kinds — not left to fail deeper and less clearly when the poller is
    /// built (DeepSeek cross-family review, 2026-08-07).
    #[tokio::test]
    async fn batch_cancel_on_a_backend_without_a_batch_lane_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        // `deepseek` is a real, configured backend whose kind has no batch lane.
        let cli = Cli::parse_from(["kaibo", "batch", "cancel", "deepseek/abc"]);
        let args = match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Cancel(c) => c,
                other => panic!("expected cancel, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        };
        let err = batch_cancel_inner(&args, &resolver)
            .await
            .expect_err("a backend with no batch lane must be refused");
        assert_eq!(err.code, EXIT_USAGE);
        assert_eq!(err.kind, "usage");
        assert!(
            err.message.contains("no batch lane"),
            "the refusal says why, not just that it failed: {}",
            err.message
        );
    }

    /// A cancel naming a backend that does not exist is a usage error before any network —
    /// the same pre-flight `batch get` applies.
    #[tokio::test]
    async fn batch_cancel_on_an_unknown_backend_is_a_usage_error_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::builtin();
        config.root = Some(dir.path().to_path_buf());
        let resolver = Resolver::from_config(Arc::new(config)).unwrap();

        let cli = Cli::parse_from(["kaibo", "batch", "cancel", "nosuchbackend/abc"]);
        let args = match cli.command {
            Some(Command::Batch(b)) => match b.cmd {
                BatchCmd::Cancel(c) => c,
                other => panic!("expected cancel, got {other:?}"),
            },
            other => panic!("expected batch, got {other:?}"),
        };
        let err = batch_cancel_inner(&args, &resolver)
            .await
            .expect_err("an unknown backend must be refused");
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
}
