//! kaibo (解剖) — an assistant agent *for other agents*.
//!
//! kaibo augments a calling agent (Claude, etc.) with a team of models, lending two
//! distinct kinds of help over MCP:
//!
//! - **Consultation** — grounded, cited answers about a codebase. A capable model
//!   reads precise spans and delegates broad sweeps to a cheap *explorer* sub-agent,
//!   all driving a read-only [`kaish`] kernel via `run_kaish(script)` (`cat`, `grep`,
//!   `rg`, `find`, `jq`, pipelines, the lot). The `consult`/`explore`/`synthesize`
//!   tools are all costumes over one primitive, [`consult::run_phase`].
//! - **Capabilities** — things the team can *do* and hand back as artifacts. The first
//!   is image generation ([`image_gen`], the `generate_image` tool); more (TTS/STT, …)
//!   follow as rig grows provider coverage. A capability is its own tool shape, not a
//!   `run_phase` loop.
//!
//! The load-bearing safety property lives in [`sandbox`]: kaibo can read the project
//! but cannot mutate it, and cannot shell out to external commands.

pub mod config;
pub mod consult;
pub mod context;
pub mod credentials;
pub mod explorer;
pub mod generate_image;
pub mod image_gen;
pub mod kaish_syntax;
pub mod mcp_log;
pub mod orientation;
pub mod progress;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod telemetry;
pub mod tls;
pub mod tool_span;
pub mod view_image;

/// A scripted, offline stand-in for a provider client, for driving the consult loop
/// deterministically in unit tests. Test-only — never compiled into the binary.
#[cfg(test)]
pub mod test_support;
