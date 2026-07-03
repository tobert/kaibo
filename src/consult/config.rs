//! The per-phase config ladder, mirroring the tool ladder it drives
//! (`run_kaish` → `explore` → `consult`, with `deliberate`'s dossier stage sharing
//! the `explore` rung). Each rung adds exactly what its phase needs on top of the
//! one below — so a phase's signature names its config type and that alone tells a
//! reader what it touches, instead of every phase sharing one bundle and carrying
//! fields inert to it (the shape this replaced: `explore`/`deliberate` filled
//! `synth_max_turns`/`attachments` with dummies just to satisfy a two-phase
//! `consult`'s type, and `oneshot` filled four).

use std::sync::Arc;
use std::time::Duration;

use crate::progress::{NullSink, ProgressSink};
use crate::sandbox::SandboxConfig;

use super::prompts::{ConsultAttachment, PromptOverrides};

/// What every model-driven phase needs, no matter how thin: preamble layering
/// (per-phase override, repo orientation, house rules — `resolve_phase_preamble`'s
/// three inputs) and where liveness goes. `oneshot` — no tools, no project reads —
/// needs exactly this and nothing more.
#[derive(Debug, Clone)]
pub struct PhaseContext {
    /// Per-phase system-prompt overrides (`[prompts]`). `Default` is empty, so the
    /// built-in preambles run unchanged. Server-set per call from the resolved
    /// config. See [`PromptOverrides`].
    pub prompts: PromptOverrides,
    /// The static repo-orientation block (assembled `[orientation]` map), or `None`.
    /// `Arc<str>` so cloning per phase is cheap. Server-set per call from the
    /// resolved root (only for the exploring tools — `explore`/`consult`); `Default`
    /// is `None`, so offline tests run the unchanged preamble.
    pub orientation: Option<Arc<str>>,
    /// Operator house rules (assembled `AGENTS.md` / user files) to splice into
    /// each top-level tool's preamble, or `None` for the historical bare preamble.
    /// `Arc<str>` so cloning per phase is cheap. The server fills it per call (it
    /// needs the resolved root to read the files); the `Default` is `None`, so
    /// every offline test runs the unchanged preamble.
    pub house_rules: Option<Arc<str>>,
    /// Where the phase's liveness goes: each delegated sweep and direct kaish read
    /// emits a [`PhaseEvent`](crate::progress::PhaseEvent) here. The server installs
    /// an adapter that renders these as MCP progress notifications when the caller
    /// asked for them; otherwise it's [`NullSink`], a no-op — so a stateless
    /// one-shot is byte-for-byte its old self.
    pub progress: Arc<dyn ProgressSink>,
    /// Wall-clock ceiling on this call's model work — the transport-agnostic backstop
    /// the per-request `request_timeout` isn't. That deadline lives in reqwest, injected
    /// through rig; when it fails to fire (a wedged local server holding a pooled
    /// keep-alive; rig's split send/body read), nothing else bounds the otherwise-
    /// brakeless prompt loop and a call can hang indefinitely (observed 2026-07-02: a
    /// stopped local backend parked a consult ~17h). This is a kaibo-owned `tokio::time`
    /// timer that doesn't trust the transport: a call past it aborts loudly rather than
    /// hanging a caller's session. Every model-driven phase carries it — that's why it
    /// rides the base rung. It bounds `consult`/`explore`/`oneshot` and the async
    /// `consult_submit`; `deliberate`'s direct lane sets it to its synth backend's own
    /// `request_timeout` (one completion), and the batch lane holds no in-process wait.
    pub call_deadline: Duration,
}

impl Default for PhaseContext {
    fn default() -> Self {
        Self {
            prompts: PromptOverrides::default(),
            orientation: None,
            house_rules: None,
            progress: Arc::new(NullSink),
            call_deadline: crate::config::Defaults::default().call_deadline,
        }
    }
}

/// What a read-only investigation needs on top of [`PhaseContext`]: sandbox limits
/// for the kaish worker(s) it spawns, and how many turns one explorer sweep gets.
/// `explore` and `deliberate`'s dossier stage need exactly this.
#[derive(Debug, Clone)]
pub struct ExploreConfig {
    /// Preamble layering and liveness — shared with every other phase.
    pub phase: PhaseContext,
    /// Bounds each explorer sweep — it's cheap, let it rip.
    pub explorer_max_turns: usize,
    /// Read-only sandbox limits applied to every kaish worker this phase spawns.
    pub sandbox: SandboxConfig,
}

impl Default for ExploreConfig {
    fn default() -> Self {
        Self {
            phase: PhaseContext::default(),
            explorer_max_turns: crate::config::Defaults::default().explorer_max_turns,
            sandbox: SandboxConfig::default(),
        }
    }
}

/// The full two-phase `consult`: an investigation ([`ExploreConfig`]) plus what
/// only the synth driver loop needs — how many turns its own loop gets, and the
/// caller's attached files (inlined within the budget, demoted to read-whole
/// directives past it).
#[derive(Debug, Clone)]
pub struct ConsultConfig {
    /// The investigation half: preamble/liveness plus explorer sweep bounds and
    /// sandbox limits, shared verbatim with the standalone `explore` tool.
    pub explore: ExploreConfig,
    /// Bounds the recomposed consult's *whole* driver loop (it delegates sweeps AND
    /// reads spans), so it must be generous — a multi-part question blew the old 8.
    pub synth_max_turns: usize,
    /// Caller-attached files, resolved server-side: attach means *the model sees the
    /// bytes*. A text file within the inline budget carries its full body (inlined
    /// numbered into the driver prompt); one past the budget is demoted to a
    /// read-it-WHOLE directive; an image routes to `view_image` (per
    /// [`ConsultAttachment`]'s variants). Every text attachment also reaches the
    /// delegated `explore′` sweeps as a preamble directive to read it whole — a sweep
    /// is a fresh agent that never saw the driver prompt. The server validates each
    /// path lands under the consult's root and classifies by content before filling
    /// this. `Default` is empty.
    pub attachments: Vec<ConsultAttachment>,
}

impl Default for ConsultConfig {
    fn default() -> Self {
        Self {
            explore: ExploreConfig::default(),
            synth_max_turns: crate::config::Defaults::default().synth_max_turns,
            attachments: Vec::new(),
        }
    }
}
