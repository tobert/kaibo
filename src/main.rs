//! kaibo (解剖) — stdio MCP server. Ask `consult` a question about a codebase;
//! kaibo explores it read-only through kaish and returns a cited answer.
//!
//! stdio only, by design: kaibo can read a filesystem, so it must never bind a
//! network socket. Logs go to stderr — stdout is the MCP transport.
//!
//! Configuration layers, highest wins: CLI flags > `KAIBO_*` env > the XDG
//! `config.toml` > built-in defaults. See `docs/config.md`. The flags here are the
//! top layer; they override whatever the config resolved.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rmcp::service::ServiceExt;
use rmcp::transport::io::stdio;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use kaibo::config::{Config, ToolDisables};
use kaibo::mcp_log::{self, McpBridgeLayer};
use kaibo::server::KaiboHandler;

#[derive(Parser)]
#[command(
    name = "kaibo",
    version,
    about = "kaibo (解剖) — read-only codebase consult MCP server (stdio)"
)]
struct Args {
    /// Path to config.toml. Overrides $KAIBO_CONFIG; default is
    /// $XDG_CONFIG_HOME/kaibo/config.toml (absent → built-in defaults).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Default project root to explore when a call omits `path`. Also joins the
    /// containment allowed set: a call's `path` must resolve to at-or-under --root
    /// or one of the --allow-path trees. With neither flag the allowed set defaults
    /// to the launch cwd (MCP clients start stdio servers with cwd = workspace), and
    /// that cwd is also adopted as the inferred default root — so a call may omit
    /// `path` without configuring anything. (An --allow-path that excludes the cwd
    /// leaves no default root, since we never default to a path containment rejects.)
    /// A leading `~` is expanded by your shell, not by kaibo; in config.toml / KAIBO_*
    /// kaibo expands it for you.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Additional allowed path tree. Repeatable. A per-call `path` must resolve
    /// to at-or-under --root or one of these; use --allow-path / to lift all
    /// limits. Also settable via KAIBO_ALLOW_PATHS (colon-separated) or
    /// [server] allow_paths in config.toml — those expand a leading `~`; on the CLI
    /// your shell does (a quoted '~/src' arrives literal and fails canonicalization).
    /// A non-empty set of --allow-path flags replaces the env/file layer.
    #[arg(long = "allow-path", value_name = "DIR", action = clap::ArgAction::Append)]
    allow_path: Vec<PathBuf>,

    /// Don't follow git worktrees: a per-call `path` in a linked worktree of an
    /// already-allowed repo is rejected like any other outside path. By default
    /// kaibo admits such worktrees (resolved by reading git's link files, never by
    /// running git), so a sibling worktree you spin up mid-session is reachable
    /// without an --allow-path. Also settable via KAIBO_NO_FOLLOW_WORKTREES or
    /// [server] follow_worktrees = false.
    #[arg(long)]
    no_follow_worktrees: bool,

    /// Default cast when a call omits it (a built-in name or a cast defined in
    /// config.toml). Built-ins: anthropic | deepseek | gemini | openai (plus
    /// aliases: claude, google, local, …). Replaces the old --provider flag
    /// (clap rejects the unknown flag loudly).
    #[arg(long)]
    cast: Option<String>,

    /// Don't advertise the `consult` tool.
    #[arg(long)]
    no_consult: bool,

    /// Don't advertise the `oneshot` tool.
    #[arg(long)]
    no_oneshot: bool,

    /// Don't advertise the `run_kaish` tool.
    #[arg(long)]
    no_run_kaish: bool,

    /// Don't advertise the `generate_image` tool.
    #[arg(long)]
    no_generate_image: bool,

    /// Don't advertise `batch_submit`. The shared `get`/`cancel`/`list` verbs remain as
    /// long as `consult` is on (they also collect consult jobs); they drop only when both
    /// `--no-batch` and `--no-consult` are set.
    #[arg(long)]
    no_batch: bool,

    /// Project house-rules file spliced into each consultation tool's preamble,
    /// resolved relative to the project root and read only if present. Repeatable.
    /// Defaults to AGENTS.md; passing any replaces that default. Also settable via
    /// KAIBO_PROJECT_FILES (colon-separated) or [context] project_files.
    #[arg(long = "project-context-file", value_name = "FILE", action = clap::ArgAction::Append)]
    project_context_file: Vec<String>,

    /// User house-rules file (e.g. ~/.claude/CLAUDE.md) spliced into each
    /// consultation tool's preamble; read unconditionally (missing is an error).
    /// Repeatable. Also settable via KAIBO_USER_FILES (colon-separated) or
    /// [context] user_files.
    #[arg(long = "user-context-file", value_name = "FILE", action = clap::ArgAction::Append)]
    user_context_file: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load config before logging so `server.log` can set the filter. A config error
    // is fatal and must be visible even though tracing isn't up yet — go to stderr.
    let config_path = args
        .config
        .clone()
        .or_else(|| std::env::var_os("KAIBO_CONFIG").map(PathBuf::from));
    let mut config = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kaibo: config error: {e:#}");
            std::process::exit(2);
        }
    };

    // CLI is the top layer: overlay it over the loaded config. `disable` carries the
    // --no-<tool> flags (true = the user asked to drop that tool). A non-empty
    // `allow_path` list replaces the env/file layer.
    config.apply_cli(
        args.root.clone(),
        args.cast.clone(),
        ToolDisables {
            consult: args.no_consult,
            oneshot: args.no_oneshot,
            run_kaish: args.no_run_kaish,
            generate_image: args.no_generate_image,
            batch: args.no_batch,
        },
        args.allow_path.clone(),
        args.no_follow_worktrees,
        args.project_context_file.clone(),
        args.user_context_file.clone(),
    );

    // Logs MUST go to stderr; stdout carries the MCP protocol. RUST_LOG wins, else
    // the config's `server.log`. The bridge layer additionally mirrors kaibo-target
    // events to the MCP `notifications/message` channel via this channel; the drain
    // task (spawned after `serve`, once the peer exists) forwards them. Records logged
    // before then — startup — buffer here and flush when draining begins, so the
    // client sees them too.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    // Per-layer filters, not one global EnvFilter. The OTLP layer must see rig's
    // `rig::*` spans — the GenAI trace tree is the whole point — while stderr and
    // the MCP bridge stay scoped to the `kaibo` target. A single global filter would
    // gate all three to one directive, so each carries its own. RUST_LOG still wins
    // over config.log (unchanged), rebuilt fresh per layer.
    let kaibo_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.log.clone()));
    // Stand up the OTLP exporter first, so the boxed layer's subscriber type is the
    // bare `Registry`. `None` (zero overhead) unless [telemetry] opted in; the guard
    // flushes the batch on exit. A build error here is fatal — a misconfigured
    // exporter is an operator mistake, surfaced loudly, not silently swallowed.
    let (otel_layer, otel_guard) =
        match kaibo::telemetry::init::<tracing_subscriber::Registry>(&config.telemetry)? {
            Some((layer, guard)) => (Some(layer), Some(guard)),
            None => (None, None),
        };
    tracing_subscriber::registry()
        .with(otel_layer)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(kaibo_filter()),
        )
        .with(McpBridgeLayer::new(log_tx).with_filter(kaibo_filter()))
        .init();

    // A zero-tool server is a misconfiguration, not a mode: refuse it loudly here,
    // before serve(), with a non-zero exit so a supervisor catches it. We prefer
    // crashing over a silently useless server.
    anyhow::ensure!(
        !config.tools.all_disabled(),
        "every tool is disabled — a zero-tool server does nothing. \
         Enable at least one (drop a --no-<tool> flag or a KAIBO_NO_<TOOL> env / \
         [server.tools] entry)."
    );

    // The resolved default cast must name a real cast — fail fast rather than
    // surface a missing cast mid-call.
    config
        .resolve_cast(&config.default_cast)
        .map_err(|e| anyhow::anyhow!("default cast: {e}"))?;

    // Any [sandbox].disable_builtins must name a builtin that actually exists in this
    // build — a typo must crash here, not silently leave the builtin enabled.
    config.validate_against_builtins(&kaibo::sandbox::builtin_names()?)?;

    tracing::info!(
        cast = %config.default_cast,
        root = ?config.root.as_ref().map(|p| p.display().to_string()),
        allow_paths = ?config.allow_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        backends = ?config.backends.keys().collect::<Vec<_>>(),
        casts = ?config.casts.keys().collect::<Vec<_>>(),
        gating = ?config.tools,
        "starting kaibo MCP server on stdio"
    );
    // A fresh install (no key, no config) starts fine but can't reach a model — say so
    // loudly here so an operator watching stderr sees it, not just the client model in
    // the handshake instructions. run_kaish still works; the model-backed tools don't.
    if matches!(
        config.default_cast_usability(|k| std::env::var(k).ok()),
        kaibo::config::CastUsability::Unconfigured
    ) {
        tracing::warn!(
            cast = %config.default_cast,
            "no API key for the default cast — consult/oneshot will fail until \
             you set a provider key (env var or key file) and reconnect; run_kaish works now. \
             See the kaibo://config/example resource."
        );
    }

    let handler = KaiboHandler::new(config)?;
    // Log the resolved (canonicalized) allowed set so the operator can verify the
    // containment boundary without inspecting config files.
    tracing::info!(
        allowed_set = ?handler.allowed_set().iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "containment boundary"
    );
    // Grab the shared log floor before `serve` consumes the handler; the drain task
    // reads it so a client `setLevel` retunes verbosity live.
    let log_level = handler.mcp_log_level();
    let service = handler.serve(stdio()).await?;
    // The peer exists now (initialize is done): start forwarding buffered + live logs.
    tokio::spawn(mcp_log::drain(log_rx, log_level, service.peer().clone()));
    service.waiting().await?;

    // Flush and stop the OTLP exporter before exit (no-op when telemetry was off):
    // the batch processor buffers spans off-thread, so without this the last spans
    // are lost. The guard's shutdown caps its own time, so a wedged collector can't
    // hang us here past the server loop.
    if let Some(guard) = otel_guard {
        guard.shutdown();
    }
    tracing::info!("kaibo shutting down");
    Ok(())
}
