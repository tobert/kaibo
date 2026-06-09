//! The progress seam — kaibo's domain events for a running phase, decoupled from
//! the MCP wire.
//!
//! A long `consult` is mostly silent: rig owns the inner LLM loop, so the only
//! places kaibo can observe forward motion are the boundaries it *does* control —
//! a tool call (`run_kaish`, a delegated `explore′` sweep), a phase start/finish,
//! the turn-cap recovery. Each emits a [`PhaseEvent`]. A [`ProgressSink`] is
//! whatever turns those into liveness: in the server it's the adapter that renders
//! them as MCP `notifications/progress` (see `server.rs`); in tests it's a
//! recording sink that asserts the deep loop actually fired them; by default it's
//! [`NullSink`], a no-op.
//!
//! Deliberately rmcp-free: the domain loop (`consult.rs`) emits semantic events
//! and never names a transport type. The translation to MCP lives at the edge.

use std::fmt;

/// A semantic step in a running phase. The sink decides how (or whether) to surface
/// it; the loop just announces what it's doing. Ordered roughly by when they fire,
/// but a sink must not assume any particular sequence — a phase may delegate zero
/// sweeps, read zero spans, or hit its turn cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseEvent {
    /// A top-level tool began (`consult` / `explore` / `synthesize`).
    PhaseStarted { phase: &'static str },
    /// The model ran a kaish script directly — a precise read.
    KaishRun { script: String },
    /// The model delegated a broad sweep to the `explore′` sub-agent.
    SweepStarted { question: String },
    /// A delegated sweep returned (it either reported or failed — both end it).
    SweepFinished,
    /// The phase exhausted its turn cap and is writing a forced final answer.
    TurnCapReached,
    /// The top-level tool finished and is about to return its answer/report.
    PhaseFinished { phase: &'static str },
}

impl PhaseEvent {
    /// A short, human-readable line for this event — the `message` field of an MCP
    /// progress notification. Long scripts/questions are clipped to one tidy line
    /// so the client gets a glanceable "what's happening now", never a wall of text.
    pub fn message(&self) -> String {
        match self {
            PhaseEvent::PhaseStarted { phase } => format!("starting {phase}"),
            PhaseEvent::KaishRun { script } => format!("running kaish: {}", brief(script, 80)),
            PhaseEvent::SweepStarted { question } => {
                format!("exploring: {}", brief(question, 80))
            }
            PhaseEvent::SweepFinished => "sweep complete".to_string(),
            PhaseEvent::TurnCapReached => "reached research limit, writing the answer".to_string(),
            PhaseEvent::PhaseFinished { phase } => format!("{phase} complete"),
        }
    }
}

/// Clip `s` to its first line, capped at `max` chars with an ellipsis. Whitespace
/// is collapsed at the edges so a script that starts with a newline still reads
/// cleanly. Counts by `char`, not byte, so a multibyte cut never splits a glyph.
fn brief(s: &str, max: usize) -> String {
    let first = s.trim().lines().next().unwrap_or("").trim();
    if first.chars().count() <= max {
        return first.to_string();
    }
    let kept: String = first.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A consumer of [`PhaseEvent`]s. Sync and infallible by design: the loop emits
/// from inside `async` tool calls and must never block or fail on a progress hop —
/// the sink fires-and-forgets (the MCP adapter spawns the actual notify). `Debug`
/// so it can ride inside `ConsultConfig` without bespoke formatting.
pub trait ProgressSink: Send + Sync + fmt::Debug {
    fn emit(&self, event: PhaseEvent);
}

/// The default sink: progress goes nowhere. What a stateless one-shot uses, and what
/// the server installs when the client didn't ask for progress (no `progressToken`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl ProgressSink for NullSink {
    fn emit(&self, _event: PhaseEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn messages_are_glanceable_per_variant() {
        assert_eq!(
            PhaseEvent::PhaseStarted { phase: "consult" }.message(),
            "starting consult"
        );
        assert_eq!(
            PhaseEvent::PhaseFinished { phase: "consult" }.message(),
            "consult complete"
        );
        assert_eq!(PhaseEvent::SweepFinished.message(), "sweep complete");
        assert!(PhaseEvent::TurnCapReached.message().contains("research limit"));
        assert_eq!(
            PhaseEvent::KaishRun { script: "rg -n TODO src".into() }.message(),
            "running kaish: rg -n TODO src"
        );
        assert_eq!(
            PhaseEvent::SweepStarted { question: "where is the sandbox?".into() }.message(),
            "exploring: where is the sandbox?"
        );
    }

    #[test]
    fn brief_clips_to_one_line_and_caps_length() {
        // Multi-line script collapses to its first line.
        assert_eq!(brief("cat -n a\nrg b\nfind c", 80), "cat -n a");
        // Leading whitespace/newlines are trimmed before the first line is taken.
        assert_eq!(brief("\n  rg -n x  \n", 80), "rg -n x");
        // Over the cap → clipped with an ellipsis, never longer than the cap.
        let long = "x".repeat(200);
        let out = brief(&long, 80);
        assert_eq!(out.chars().count(), 80, "clip keeps max chars incl. the ellipsis");
        assert!(out.ends_with('…'), "an over-length clip is marked with an ellipsis");
    }

    #[test]
    fn brief_handles_multibyte_without_splitting_a_glyph() {
        // 100 kanji, cap 10 → 9 kanji + ellipsis, and it must not panic on a byte cut.
        let kanji = "解".repeat(100);
        let out = brief(&kanji, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn null_sink_swallows_events() {
        // No panic, no state — the no-op contract.
        NullSink.emit(PhaseEvent::SweepFinished);
    }

    /// `Arc<dyn ProgressSink>` must be `Debug` (it rides inside `ConsultConfig`,
    /// which derives `Debug`). A trait-object that lost its `Debug` supertrait would
    /// fail to compile here — that's the teeth.
    #[test]
    fn dyn_sink_is_debug_behind_arc() {
        let sink: Arc<dyn ProgressSink> = Arc::new(NullSink);
        let _ = format!("{sink:?}");
    }
}
