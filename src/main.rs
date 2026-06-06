//! kaibo (解剖) — stdio MCP server. Ask `consult` a question about a codebase;
//! kaibo explores it read-only through kaish and returns a cited answer.
//!
//! stdio only, by design: kaibo can read a filesystem, so it must never bind a
//! network socket. Logs go to stderr — stdout is the MCP transport.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rmcp::service::ServiceExt;
use rmcp::transport::io::stdio;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use kaibo::credentials::Provider;
use kaibo::server::{KaiboHandler, ToolGating};

#[derive(Parser)]
#[command(name = "kaibo", version, about = "kaibo (解剖) — read-only codebase consult MCP server (stdio)")]
struct Args {
    /// Default project root to explore when a call omits `path`.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Default provider when a call omits it: anthropic | deepseek | gemini | lemonade.
    #[arg(long, default_value = "anthropic")]
    provider: String,

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
    // Logs MUST go to stderr; stdout carries the MCP protocol.
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("kaibo=info")),
        )
        .init();

    let args = Args::parse();
    let provider: Provider = args.provider.parse()?;

    if let Some(root) = &args.root {
        anyhow::ensure!(root.is_dir(), "--root is not a directory: {}", root.display());
    }

    let gating = ToolGating {
        consult: !args.no_consult,
        explore: !args.no_explore,
        synthesize: !args.no_synthesize,
        run_kaish: !args.no_run_kaish,
    };
    // A zero-tool server is a misconfiguration, not a mode: refuse it loudly here,
    // before serve(), with a non-zero exit so a supervisor catches it. We prefer
    // crashing over a silently useless server.
    anyhow::ensure!(
        !gating.all_disabled(),
        "all four tools are disabled — a zero-tool server does nothing. \
         Drop at least one --no-<tool> flag (consult/explore/synthesize/run_kaish)."
    );

    tracing::info!(
        provider = ?provider,
        root = ?args.root.as_ref().map(|p| p.display().to_string()),
        gating = ?gating,
        "starting kaibo MCP server on stdio"
    );

    let handler = KaiboHandler::new(args.root, provider, gating);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("kaibo shutting down");
    Ok(())
}
