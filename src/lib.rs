//! kaibo (解剖) — a two-phase MCP consult agent.
//!
//! An MCP client asks `kaibo` a question. Internally, a cheap *explorer* model
//! drives a read-only [`kaish`] kernel via a single `run_kaish(script)` tool —
//! `cat`, `grep`, `rg`, `find`, `jq`, `awk`, pipelines, the lot — and writes a
//! curated report. A *synthesizer* model then answers from that report, with the
//! same tools available as a fallback for precise spans.
//!
//! The load-bearing safety property lives in [`sandbox`]: the explorer can read
//! the project but cannot mutate it, and cannot shell out to external commands.

pub mod config;
pub mod consult;
pub mod credentials;
pub mod explorer;
pub mod kaish_syntax;
pub mod mcp_log;
pub mod progress;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod view_image;

/// A scripted, offline stand-in for a provider client, for driving the consult loop
/// deterministically in unit tests. Test-only — never compiled into the binary.
#[cfg(test)]
pub mod test_support;
