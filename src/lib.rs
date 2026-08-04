//! kaibo — an assistant agent *for other agents*.
//!
//! kaibo augments a calling agent (Claude, etc.) with a team of models, lending one
//! kind of help over MCP: **consultation** — grounded, cited answers about a codebase.
//! A **synthesis agent** reads precise spans and delegates broad sweeps to a cheap *explorer*
//! sub-agent, all driving a read-only [`kaish`] kernel via `run_kaish(script)` (`cat`,
//! `grep`, `find`, `jq`, pipelines, the lot). The `consult` and toolless `oneshot` tools
//! are both costumes over one primitive, [`consult::run_phase`]. The team *perceives*
//! what fuses into its reasoning (image input today, via `view_image`); when kaibo
//! *produces* (the `generate` tool, via [`media`]), the artifacts land only in the
//! content-addressed media store ([`cas`]) — an individually-gated mediated surface at
//! a fixed XDG path no model steers, never a general write path.
//!
//! The load-bearing safety property lives in [`sandbox`]: kaibo can read the project
//! but cannot mutate it — the project is untouchable from every path (the model-facing
//! shell has no write mount; kaibo's own two blessed write surfaces, the persistence
//! store and the media CAS, refuse to resolve into any allowed tree) — and cannot
//! shell out to external commands.

pub mod acp;
pub mod attach;
pub mod batch;
pub mod cas;
pub mod cli;
pub mod completion_watch;
pub mod config;
pub mod consult;
pub mod context;
pub mod credentials;
pub mod discover;
pub mod explorer;
pub mod jobs;
pub mod kaish_syntax;
pub mod mcp_log;
pub mod media;
pub mod openai_images;
pub mod orientation;
pub mod progress;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod stability;
pub mod store;
pub mod sweep_attach;
pub mod telemetry;
pub mod tls;
pub mod tool_span;
pub mod view_image;
pub mod worktree;

/// A scripted, offline stand-in for a provider client, for driving the consult loop
/// deterministically in unit tests. Test-only — never compiled into the binary.
#[cfg(test)]
pub mod test_support;
