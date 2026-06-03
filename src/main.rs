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
use kaibo::server::KaiboHandler;

#[derive(Parser)]
#[command(name = "kaibo", version, about = "kaibo (解剖) — read-only codebase consult MCP server (stdio)")]
struct Args {
    /// Default project root to explore when a `consult` call omits `path`.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Default provider when a call omits it: anthropic | deepseek | gemini.
    #[arg(long, default_value = "anthropic")]
    provider: String,
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

    tracing::info!(
        provider = ?provider,
        root = ?args.root.as_ref().map(|p| p.display().to_string()),
        "starting kaibo MCP server on stdio"
    );

    let handler = KaiboHandler::new(args.root, provider);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("kaibo shutting down");
    Ok(())
}
