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
//!   provenance footer the MCP tool returns) goes to stdout; progress beats and logs
//!   go to stderr (via [`TerminalSink`](crate::progress::TerminalSink)). `--json`
//!   swaps stdout for a structured envelope (answer + provenance + usage).
//! - **exit codes have teeth.** `0` = an answer; `2` = a usage/config error; `3` = a
//!   containment or setup rejection (bad path/cast/attachment/key); `4` = the
//!   consultation itself failed (provider/model-loop). So an agent branches on the
//!   code without parsing prose.
//!
//! `--help` is model-facing text: an agent reads it the way an MCP client reads a
//! tool description, so the top-level `about` front-loads what kaibo is and every
//! flag doc earns its line (the "Writing for models" discipline).

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use rmcp::ErrorData as McpError;

use crate::config::{Config, ModelRole, ToolDisables};
use crate::consult::{consult, ConsultConfig, ConsultOutput, ExploreConfig, PhaseContext};
use crate::progress::TerminalSink;
use crate::server::{consultation_failure_text, render_config_resource, with_provenance, Resolver};
use crate::session::{SessionStore as MemSessionStore, Sessions};

/// Exit codes, distinct so an agent caller branches without parsing prose.
pub const EXIT_OK: i32 = 0;
/// A usage or config-load error (also clap's own arg-error code).
pub const EXIT_USAGE: i32 = 2;
/// A containment or setup rejection: bad `--path`/`--root`, unknown cast, an
/// attachment outside the boundary, a missing provider key.
pub const EXIT_SETUP: i32 = 3;
/// The consultation ran but failed (provider overload, model-loop error, timeout).
pub const EXIT_CONSULT_FAILURE: i32 = 4;

/// kaibo (解剖) — read-only codebase consultation from a model outside your own
/// family. Ask a question; a capable model (DeepSeek, Gemini, Anthropic, OpenRouter,
/// or local — pick with `--cast`) reads the project READ-ONLY and answers with
/// `file:line` citations, never modifying anything. Bare `kaibo` is the MCP server
/// (stdio); `kaibo consult` is the one-shot CLI; `kaibo config` prints the resolved
/// configuration.
#[derive(Parser, Debug)]
#[command(name = "kaibo", version)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The implicit `serve` flags: a bare `kaibo` (no subcommand) runs the MCP
    /// server, so every existing client config keeps working unchanged.
    #[command(flatten)]
    pub serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
// The parsed CLI lives for one `main` dispatch, so the size gap between the arg
// variants is irrelevant — and clap's derive can't parse a `Box<Args>` variant.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Run the MCP server on stdio (the explicit form of a bare `kaibo`).
    Serve(ServeArgs),
    /// Ask one read-only consultation question; the cited answer prints to stdout.
    Consult(ConsultArgs),
    /// Print the resolved runtime configuration (the `kaibo://config` document).
    Config(CommonArgs),
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

/// The MCP server flags: the shared set plus the per-tool `--no-<tool>` gates (which
/// only make sense for the long-lived server).
#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    #[command(flatten)]
    pub common: CommonArgs,

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

impl ServeArgs {
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

/// `kaibo consult` — one read-only consultation.
#[derive(Args, Debug)]
pub struct ConsultArgs {
    #[command(flatten)]
    pub common: CommonArgs,

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

    /// Emit a JSON envelope on stdout (answer + provenance + usage) instead of prose,
    /// for a script caller.
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

/// Print `err` to stderr and return `code` — the shared shape for a setup/usage
/// rejection (before the model runs). With `--json`, the message rides a structured
/// envelope on stdout so a script parses it uniformly with a success envelope.
fn fail_setup(json: bool, kind: &str, message: String, code: i32) -> i32 {
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
pub async fn run_consult(args: ConsultArgs) -> i32 {
    init_cli_logging();

    let config = match load_config(&args.common) {
        Ok(c) => c,
        Err(e) => {
            return fail_setup(
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
            return fail_setup(args.json, "setup", format!("{e:#}"), EXIT_SETUP);
        }
    };

    // Resolution stage — every refusable thing (bad path/cast/key/attachment) is an
    // EXIT_SETUP rejection, distinct from a consultation that ran and failed.
    let outcome = resolve_and_run(
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
        }) => fail_setup(args.json, kind, message, code),
    }
}

/// A setup-stage rejection carrying the exit code it maps to.
struct SetupError {
    kind: &'static str,
    message: String,
    code: i32,
}

impl From<McpError> for SetupError {
    fn from(e: McpError) -> Self {
        SetupError {
            kind: "setup",
            message: e.message.to_string(),
            code: EXIT_SETUP,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_and_run(
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
    let root = resolver.resolve_root(args.path.clone())?;
    let mut cast = resolver.resolve_cast(args.common.cast.clone())?;
    resolver.reject_offline_cast(&cast, "consult")?;
    resolver.apply_model_override(
        &mut cast,
        ModelRole::Explorer,
        args.explorer_model.as_deref(),
        args.explorer_backend.as_deref(),
        "explorer_model",
        "explorer_backend",
    )?;
    resolver.apply_model_override(
        &mut cast,
        ModelRole::Synth,
        args.synth_model.as_deref(),
        args.synth_backend.as_deref(),
        "synth_model",
        "synth_backend",
    )?;
    let explorer = resolver.arm(&cast, ModelRole::Explorer)?;
    let synth = resolver.arm(&cast, ModelRole::Synth)?;

    let attachments =
        Resolver::resolve_consult_attachments(&root, &args.attach, inline_budget, sandbox).await?;
    Resolver::gate_consult_image_attachments(
        &attachments,
        synth.caps.vision,
        &synth.model,
        &cast.name,
    )?;

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
                house_rules: resolver.house_rules(&root)?,
                prompts: resolver.resolved_prompts(&cast),
                orientation: resolver.orientation(&root).await?,
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
        // failed, distinct from a setup rejection. Reuse the server's classified text.
        Err(e) => {
            let text = consultation_failure_text("consult", &cast.name, e);
            if args.json {
                println!("{}", error_envelope("consultation_failure", &text));
            } else {
                eprintln!("kaibo: {text}");
            }
            Ok(EXIT_CONSULT_FAILURE)
        }
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
                 Fix the path/permissions, or pass --no-persistence.",
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
    // The report is opt-in extra; keep it off stdout's answer line — send it to stderr
    // so a pipe still captures just the answer.
    if args.include_report && !out.report.is_empty() {
        eprintln!("\n--- explorer report ---\n{}", out.report);
    }
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
        "answer": out.answer,
        "cast": cast,
        "models": { "explorer": explorer_model, "synth": synth_model },
        "usage": {
            "input_tokens": out.usage.input_tokens,
            "output_tokens": out.usage.output_tokens,
            "reasoning_tokens": out.usage.reasoning_tokens,
            "cached_input_tokens": out.usage.cached_input_tokens,
            "cache_creation_input_tokens": out.usage.cache_creation_input_tokens,
        },
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
        // No subcommand → the server path; the flattened serve args carry the flags.
        let cli = Cli::try_parse_from(["kaibo", "--root", "/tmp"]).expect("bare parse");
        assert!(
            cli.command.is_none(),
            "bare kaibo has no subcommand (implicit serve)"
        );
        assert_eq!(
            cli.serve.common.root.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    #[test]
    fn explicit_serve_takes_the_same_flags() {
        let cli = Cli::try_parse_from(["kaibo", "serve", "--no-oneshot", "--cast", "gemini"])
            .expect("serve parse");
        match cli.command {
            Some(Command::Serve(s)) => {
                assert!(s.no_oneshot);
                assert_eq!(s.common.cast.as_deref(), Some("gemini"));
                assert!(s.tool_disables().oneshot);
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
        match cli.command {
            Some(Command::Consult(c)) => {
                assert_eq!(c.question, "why is this slow?");
                assert_eq!(c.common.cast.as_deref(), Some("deepseek"));
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
        assert!(matches!(cli.command, Some(Command::Config(_))));
    }

    #[test]
    fn a_missing_consult_question_is_a_usage_error() {
        // The positional question is required — clap rejects its absence (exit 2 class).
        let err = Cli::try_parse_from(["kaibo", "consult"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn consult_envelope_carries_answer_provenance_and_usage() {
        let out = ConsultOutput {
            answer: "the answer".to_string(),
            report: "the report".to_string(),
            usage: rig_core::completion::Usage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
        };
        let env = consult_envelope(&out, "deepseek", "explorer-m", "synth-m", false);
        assert_eq!(env["answer"], "the answer");
        assert_eq!(env["cast"], "deepseek");
        assert_eq!(env["models"]["explorer"], "explorer-m");
        assert_eq!(env["models"]["synth"], "synth-m");
        assert_eq!(env["usage"]["input_tokens"], 10);
        assert_eq!(env["usage"]["output_tokens"], 20);
        // The report is omitted unless requested.
        assert!(env.get("report").is_none(), "report is opt-in");

        let with_report = consult_envelope(&out, "deepseek", "e", "s", true);
        assert_eq!(with_report["report"], "the report");
    }

    #[test]
    fn error_envelope_shape() {
        let env = error_envelope("consultation_failure", "it broke");
        assert_eq!(env["kind"], "consultation_failure");
        assert_eq!(env["error"], "it broke");
    }
}
