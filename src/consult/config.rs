//! The per-phase configs. Every phase shares [`PhaseContext`]; each phase's own type
//! adds exactly what that phase needs ([`ExploreConfig`] for `deliberate`'s dossier
//! survey, [`ConsultConfig`] for the consult driver, [`ResearchConfig`] for `research`),
//! so a phase's signature names its config type and that alone tells a reader what it
//! touches, instead of every phase sharing one bundle and carrying fields inert to it.

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
    /// resolved root (only for the project-reading phases — `consult` and `deliberate`'s
    /// dossier survey); `Default`
    /// is `None`, so offline tests run the unchanged preamble.
    pub orientation: Option<Arc<str>>,
    /// Operator house rules (assembled `AGENTS.md` / user files) to splice into
    /// each top-level tool's preamble, or `None` for the historical bare preamble.
    /// `Arc<str>` so cloning per phase is cheap. The server fills it per call (it
    /// needs the resolved root to read the files); the `Default` is `None`, so
    /// every offline test runs the unchanged preamble.
    pub house_rules: Option<Arc<str>>,
    /// Where the phase's liveness goes: each kaish read
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
    /// rides the base rung. It bounds `consult`/`oneshot`, the dossier survey, and the async
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
/// `deliberate`'s dossier stage needs exactly this, and `consult` carries it for the
/// shared fields (preamble layering, liveness, sandbox, web).
#[derive(Debug, Clone)]
pub struct ExploreConfig {
    /// Preamble layering and liveness — shared with every other phase.
    pub phase: PhaseContext,
    /// Bounds each explorer sweep.
    pub explorer_max_turns: usize,
    /// Read-only sandbox limits applied to every kaish worker this phase spawns.
    pub sandbox: SandboxConfig,
    /// Cap on how many files ONE sweep may route with its `attach` tool — the bytes
    /// ride ALONGSIDE the sweep's report to whoever reads it, without entering the
    /// explorer's own context. Only `deliberate`'s dossier stage reads it. `0` means
    /// "don't inject the tool at all".
    pub max_attachments: usize,
    /// The call's web session, when the caller asked for the web (`web: true`) and the
    /// operator allows it. `Some` gives the phase (the explorer survey, or the consult
    /// driver) `web_search` and `web_fetch` plus the web directive, all drawing on the one budget. `None`
    /// (the default) is byte-for-byte the project-only survey.
    pub web: Option<Arc<crate::web::WebSession>>,
}

impl Default for ExploreConfig {
    fn default() -> Self {
        Self {
            phase: PhaseContext::default(),
            explorer_max_turns: crate::config::Defaults::default().explorer_max_turns,
            sandbox: SandboxConfig::default(),
            max_attachments: crate::config::Defaults::default().max_attachments,
            web: None,
        }
    }
}

/// What the `research` survey needs: preamble layering and liveness, the turn bound
/// (the explorer's, since research runs on the explorer slot), and the call's web
/// session, which is required here rather than optional because web access is the
/// whole phase.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    pub phase: PhaseContext,
    pub max_turns: usize,
    pub web: Arc<crate::web::WebSession>,
}

/// The `consult` driver's config: preamble layering and liveness, the sandbox limits
/// and web session its reads run under, how many turns its loop gets, and the caller's
/// attached files (inlined within the budget, demoted to read-whole directives past it).
#[derive(Debug, Clone)]
pub struct ConsultConfig {
    /// Preamble layering and liveness — shared with every other phase.
    pub phase: PhaseContext,
    /// Read-only sandbox limits for the driver's kaish worker.
    pub sandbox: SandboxConfig,
    /// The call's web session, when the caller asked for the web (`web: true`) and the
    /// operator allows it. `None` (the default) is the project-only consult.
    pub web: Option<Arc<crate::web::WebSession>>,
    /// Bounds the consult's *whole* driver loop (every read is its own), so it must
    /// be generous — a multi-part question blew the old 8.
    pub synth_max_turns: usize,
    /// Caller-attached files, resolved server-side: attach means *the model sees the
    /// bytes*. A text file within the inline budget carries its full body (inlined
    /// numbered into the driver prompt); one past the budget is demoted to a
    /// read-it-WHOLE directive; an image routes to `view_image` (per
    /// [`ConsultAttachment`]'s variants). The server validates each
    /// path lands under the consult's root and classifies by content before filling
    /// this. `Default` is empty.
    pub attachments: Vec<ConsultAttachment>,
    /// Where the driver's `save_artifact` calls land, or `None` (the default) for no
    /// such tool in the toolset at all.
    ///
    /// The server builds one sink per MCP call, and only when all three keys hold: the
    /// operator enabled `[artifacts]`, this call asked with `save_artifacts`, and the
    /// media CAS is live. `None` is byte-for-byte the pre-artifact consult.
    ///
    /// [`ExploreConfig`] has no such field, and that is the enforcement of "driver-loop
    /// only": `deliberate`'s dossier explorer cannot be handed the tool by accident.
    pub artifacts: Option<Arc<crate::artifact::ArtifactSink>>,
}

impl Default for ConsultConfig {
    fn default() -> Self {
        Self {
            phase: PhaseContext::default(),
            sandbox: SandboxConfig::default(),
            web: None,
            synth_max_turns: crate::config::Defaults::default().synth_max_turns,
            attachments: Vec::new(),
            artifacts: None,
        }
    }
}
