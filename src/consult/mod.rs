//! `consult` and the seams it's composed from, across providers.
//!
//! One primitive — [`run_phase`]: a model + preamble + an injected toolset, run as
//! a bounded tool loop. Every tool on the surface is that loop wearing different
//! clothes:
//!
//! - [`consult`] — a capable model · `{run_kaish, explore′}` · optional context → a
//!   cited answer. No rigid explorer→synth hand-off: the capable model decides when
//!   to delegate a broad sweep to the cheap [`RunExplore`] sub-agent vs. read a span
//!   directly. The `explore′` sweep ([`report_preamble`]) is internal to this loop.
//! - [`oneshot`] — a capable model · no tools · the caller's context → a direct
//!   answer. The thin counterpart: one upstream request, no codebase access.
//!
//! Each phase arrives as a resolved [`Arm`]: its own client (type-erased — the
//! decided plumbing fork, `docs/casts.md`), model, request params, and caps. The
//! server resolves a cast's slots into arms ([`Arm::from_slot`]); a cast whose
//! explorer and synth live on different backends — different wire protocols,
//! even — runs each phase on its own client through the same loop primitive.
//! Each tool gets its own fresh [`KaishWorker`] (a kernel rooted at the
//! project), and so does every `explore′` delegation.

mod config;
mod engine;
mod prompts;
mod shaping;

pub use config::{ConsultConfig, ExploreConfig, PhaseContext};
pub(crate) use engine::explore_with;
pub use engine::{
    consult, deliberate_direct, oneshot, Arm, ConsultOutput, RunExplore, RunExploreArgs,
    RunExploreError, Session,
};
// `run_phase` is the offline-testable loop primitive; re-exported at crate scope so
// `crate::consult::run_phase` (referenced in module docs) resolves. No in-crate `use`
// imports it by that path today, so silence the re-export's unused-import lint.
#[allow(unused_imports)]
pub(crate) use engine::run_phase;
pub use prompts::{
    batch_preamble, batch_system_prompt, consult_preamble, consult_user_prompt,
    deliberation_prompt, oneshot_preamble, report_preamble, resolve_phase_preamble,
    ConsultAttachment, Phase, PromptOverrides,
};
pub use shaping::{
    inject_provider_prefs, request_params, thinking_params, ModelCaps, ModelShape,
    ThinkingStyleOverride, DEFAULT_EFFORT, THINKING_BUDGET,
};
