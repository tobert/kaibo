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
#[command(name = "kaibo", version, about = "kaibo (解剖) — read-only codebase consult MCP server (stdio)")]
struct Args {
    /// Path to config.toml. Overrides $KAIBO_CONFIG; default is
    /// $XDG_CONFIG_HOME/kaibo/config.toml (absent → built-in defaults).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Default project root to explore when a call omits `path`.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Default provider/profile when a call omits it (a built-in kind name or a
    /// profile defined in config.toml). Built-ins: anthropic | deepseek | gemini |
    /// openai.
    #[arg(long)]
    provider: Option<String>,

    /// Don't advertise the `consult` tool.
    #[arg(long)]
    no_consult: bool,

    /// Don't advertise the `explore` tool.
    #[arg(long)]
    no_explore: bool,

    /// Don't advertise the `synthesize` tool.
    #[arg(long)]
    no_synthesize: bool,

    /// Don't advertise the `run_kaish` tool.
    #[arg(long)]
    no_run_kaish: bool,
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
    // --no-<tool> flags (true = the user asked to drop that tool).
    config.apply_cli(
        args.root.clone(),
        args.provider.clone(),
        ToolDisables {
            consult: args.no_consult,
            explore: args.no_explore,
            synthesize: args.no_synthesize,
            run_kaish: args.no_run_kaish,
        },
    );

    // Logs MUST go to stderr; stdout carries the MCP protocol. RUST_LOG wins, else
    // the config's `server.log`. The bridge layer additionally mirrors kaibo-target
    // events to the MCP `notifications/message` channel via this channel; the drain
    // task (spawned after `serve`, once the peer exists) forwards them. Records logged
    // before then — startup — buffer here and flush when draining begins, so the
    // client sees them too.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(config.log.clone())),
        )
        .with(McpBridgeLayer::new(log_tx))
        .init();

    if let Some(root) = &config.root {
        anyhow::ensure!(root.is_dir(), "root is not a directory: {}", root.display());
    }

    // A zero-tool server is a misconfiguration, not a mode: refuse it loudly here,
    // before serve(), with a non-zero exit so a supervisor catches it. We prefer
    // crashing over a silently useless server.
    anyhow::ensure!(
        !config.tools.all_disabled(),
        "all four tools are disabled — a zero-tool server does nothing. \
         Enable at least one (drop a --no-<tool> flag or a KAIBO_NO_<TOOL> env / \
         [server.tools] entry)."
    );

    // The resolved default provider must name a real profile — fail fast rather than
    // surface a missing profile mid-call.
    config
        .resolve_profile(&config.default_provider)
        .map_err(|e| anyhow::anyhow!("default provider: {e}"))?;

    // Any [sandbox].disable_builtins must name a builtin that actually exists in this
    // build — a typo must crash here, not silently leave the builtin enabled.
    config.validate_against_builtins(&kaibo::sandbox::builtin_names()?)?;

    tracing::info!(
        provider = %config.default_provider,
        root = ?config.root.as_ref().map(|p| p.display().to_string()),
        profiles = ?config.profiles.keys().collect::<Vec<_>>(),
        gating = ?config.tools,
        "starting kaibo MCP server on stdio"
    );

    let handler = KaiboHandler::new(config)?;
    // Grab the shared log floor before `serve` consumes the handler; the drain task
    // reads it so a client `setLevel` retunes verbosity live.
    let log_level = handler.mcp_log_level();
    let service = handler.serve(stdio()).await?;
    // The peer exists now (initialize is done): start forwarding buffered + live logs.
    tokio::spawn(mcp_log::drain(log_rx, log_level, service.peer().clone()));
    service.waiting().await?;

    tracing::info!("kaibo shutting down");
    Ok(())
}
