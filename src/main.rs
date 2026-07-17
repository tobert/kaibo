//! kaibo (解剖) — read-only codebase consultation, as a stdio MCP server and a CLI.
//!
//! Bare `kaibo` (and `kaibo serve`) is the MCP server: ask `consult` a question about
//! a codebase; kaibo explores it read-only through kaish and returns a cited answer.
//! stdio only, by design — kaibo can read a filesystem, so it must never bind a
//! network socket. Logs go to stderr; stdout carries the MCP transport.
//!
//! `kaibo consult "…"` and `kaibo config` are the CLI front door (see `kaibo::cli`),
//! for CLI-first agents, scripts, and humans without an MCP client. Every existing
//! MCP client config keeps working unchanged: a bare invocation is still the server.
//!
//! Configuration layers, highest wins: CLI flags > `KAIBO_*` env > the XDG
//! `config.toml` > built-in defaults. See `docs/config.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rmcp::service::ServiceExt;
use rmcp::transport::io::stdio;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use kaibo::cli::{Cli, Command, CommonArgs, ServeGates};
use kaibo::config::Config;
use kaibo::mcp_log::{self, McpBridgeLayer};
use kaibo::server::KaiboHandler;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The shared flags live on `cli.common` (globals, usable before or after the
    // subcommand); each arm consumes them alongside its own payload.
    match cli.command {
        // Bare `kaibo` and explicit `kaibo serve` both run the MCP server on the same
        // flags — the compatibility contract for existing client configs.
        None => serve(cli.common, cli.gates).await,
        Some(Command::Serve(gates)) => serve(cli.common, gates).await,
        // The CLI front doors own their exit codes (0 answer / 2 usage / 3 setup /
        // 4 consultation failure), so they return a code and we exit on it.
        Some(Command::Consult(args)) => {
            std::process::exit(kaibo::cli::run_consult(cli.common, args).await)
        }
        Some(Command::Config) => std::process::exit(kaibo::cli::run_config(cli.common)),
    }
}

/// Run the MCP server on stdio. The body of a bare `kaibo` / `kaibo serve` — behavior
/// is byte-for-byte what it was before the CLI restructure. `common` carries the shared
/// flags; `gates` the serve-only `--no-<tool>` toggles.
async fn serve(common: CommonArgs, gates: ServeGates) -> Result<()> {
    // Load config before logging so `server.log` can set the filter. A config error
    // is fatal and must be visible even though tracing isn't up yet — go to stderr.
    let config_path = common
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

    // CLI is the top layer: overlay it over the loaded config. A non-empty `allow_path`
    // list replaces the env/file layer.
    config.apply_cli(
        common.root.clone(),
        common.cast.clone(),
        gates.tool_disables(),
        common.allow_path.clone(),
        common.no_follow_worktrees,
        common.project_context_file.clone(),
        common.user_context_file.clone(),
        common.no_persistence,
        common.state_db.clone(),
    );

    // Logs MUST go to stderr; stdout carries the MCP protocol. RUST_LOG wins, else
    // the config's `server.log`. The bridge layer additionally mirrors kaibo-target
    // events to the MCP `notifications/message` channel via this channel; the drain
    // task (spawned after `serve`, once the peer exists) forwards them. Records logged
    // before then — startup — buffer here and flush when draining begins, so the
    // client sees them too.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    // The pull-side ring: the bridge layer tees kaibo-target records here so the `job_wait`
    // tool can drain them on demand (the same records it streams to the client). Created
    // before the subscriber so the layer and the handler share one ring. Bounded — old
    // records age out; it's a convenience, not the authoritative job state.
    let notifications = mcp_log::NotificationBuffer::new(512);
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
        .with(
            McpBridgeLayer::new(log_tx)
                .with_buffer(notifications.clone())
                .with_filter(kaibo_filter()),
        )
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

    // Capture the persistence settings before `config` moves into the handler.
    let persistence = config.persistence.clone();
    let session_capacity = config.defaults.session_capacity;
    let handler = KaiboHandler::new(config)?.with_notifications(notifications);

    // Stand up the durable session/batch store when persistence is enabled. Opening it is
    // fallible (a bad path, a locked db, a network mount). A failure is a LOUD startup error
    // naming the escape hatch — never a silent fallback to memory (crash over silent
    // fallback) — with ONE narrow, deliberate carve-out below. The store is fed the handler's
    // resolved allowed set so its containment guard refuses a state db inside any project
    // tree. When persistence is off, the handler keeps its in-memory sessions unchanged.
    let handler = if persistence.enabled {
        let path = persistence
            .path
            .expect("an enabled persistence store has a resolved path (validated at load)");
        let allowed = handler.allowed_set();
        let allowed_refs: Vec<&std::path::Path> = allowed.iter().map(PathBuf::as_path).collect();
        match kaibo::store::SessionStore::open(&path, session_capacity, &allowed_refs).await {
            Ok(store) => {
                tracing::info!(
                    state_db = %path.display(),
                    "persistence enabled — sessions and batch handles are durable across restarts"
                );
                handler.with_session_store(store)
            }
            // THE ONE CARVE-OUT (Windows / single-process platforms only): another kaibo
            // already holds the db. MP-WAL is 64-bit-Unix-only, so a `SingleProcessLocked`
            // can't arise on the Unix path — this is a Windows second-editor-window case.
            // Crashing there would crash-LOOP under MCP clients that auto-restart servers, so
            // we WARN loudly and serve with in-memory sessions instead. Not silent: the
            // startup log says so, and `kaibo://config` shows `persistence.active = false`
            // with `enabled = true`, which the calling model can read. Every OTHER open error
            // (below) stays fatal-and-loud. (Flagged for Amy's review at the PR — this amends
            // the loud-fail posture for exactly this one case.)
            Err(e @ kaibo::store::StoreError::SingleProcessLocked(_)) => {
                tracing::warn!(
                    state_db = %path.display(),
                    "{e} — continuing with IN-MEMORY sessions (this run will NOT persist; \
                     kaibo://config shows persistence.active=false). Close the other kaibo, \
                     point --state-db elsewhere, or set --no-persistence to silence this."
                );
                handler
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to open the persistence state db at {}: {e}\n\
                     The state db is a convenience layer — if it's corrupt it is safe to \
                     delete by hand (kaibo creates a fresh empty one on the next start; it \
                     never auto-deletes, since the error could be transient). Otherwise fix \
                     the path/permissions, or run without persistence (--no-persistence, \
                     KAIBO_NO_PERSISTENCE, or [persistence] enabled = false in config.toml).",
                    path.display()
                ));
            }
        }
    } else {
        tracing::info!(
            "persistence disabled — sessions are in-memory (lost on restart), \
             batch handles held nowhere"
        );
        handler
    };
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
