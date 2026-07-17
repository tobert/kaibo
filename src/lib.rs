//! kaibo (解剖) — an assistant agent *for other agents*.
//!
//! kaibo augments a calling agent (Claude, etc.) with a team of models, lending one
//! kind of help over MCP: **consultation** — grounded, cited answers about a codebase.
//! A capable model reads precise spans and delegates broad sweeps to a cheap *explorer*
//! sub-agent, all driving a read-only [`kaish`] kernel via `run_kaish(script)` (`cat`,
//! `grep`, `find`, `jq`, pipelines, the lot). The `consult` and toolless `oneshot` tools
//! are both costumes over one primitive, [`consult::run_phase`]. The team *perceives*
//! what fuses into its reasoning (image input today, via `view_image`); it produces no
//! output artifacts — kaibo reasons over code, it doesn't render or emit.
//!
//! The load-bearing safety property lives in [`sandbox`]: kaibo can read the project but
//! cannot mutate it (read-only is *unconditional* — no write path of any kind), and
//! cannot shell out to external commands.

pub mod attach;
pub mod batch;
pub mod cli;
pub mod config;
pub mod consult;
pub mod context;
pub mod credentials;
pub mod explorer;
pub mod jobs;
pub mod kaish_syntax;
pub mod mcp_log;
pub mod orientation;
pub mod progress;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod store;
pub mod telemetry;
pub mod tls;
pub mod tool_span;
pub mod view_image;
pub mod worktree;

/// A scripted, offline stand-in for a provider client, for driving the consult loop
/// deterministically in unit tests. Test-only — never compiled into the binary.
#[cfg(test)]
pub mod test_support;
