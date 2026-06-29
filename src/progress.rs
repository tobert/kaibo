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
use std::sync::Arc;
use std::sync::Mutex;

/// A semantic step in a running phase. The sink decides how (or whether) to surface
/// it; the loop just announces what it's doing. Ordered roughly by when they fire,
/// but a sink must not assume any particular sequence — a phase may delegate zero
/// sweeps, read zero spans, or hit its turn cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseEvent {
    /// A top-level tool began (`consult` / `oneshot`).
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

/// A [`ProgressSink`] that maps each [`PhaseEvent`] onto kaibo's `tracing` stream under
/// the `kaibo::consult` target. An *async* phase has no live MCP peer to push progress
/// notifications to, so this is how it stays legible: the `mcp_log` bridge mirrors these
/// `tracing` events to a watching client (the live "watch it work" view sync `consult`
/// gave), and the notification ring buffer tees them for `wait`.
///
/// Levels follow kaibo's convention — **Warn = "promote to the calling model"**, Info =
/// the watchable narrative — *not* severity:
/// - `KaishRun`, the sweep events, and phase start/finish → **Info**: each shell command
///   and milestone, the user's continuous view.
/// - `TurnCapReached` → **Warn**: the caller should know the research budget ran out and
///   the answer was written early, so it surfaces in the model's `wait` drain.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingSink;

impl ProgressSink for TracingSink {
    fn emit(&self, event: PhaseEvent) {
        // `event.message()` is the same tidy one-liner sync consult streamed; reuse it so
        // the two channels read identically. The level branch is the only divergence.
        let msg = event.message();
        if promotes_to_caller(&event) {
            tracing::warn!(target: "kaibo::consult", "{msg}");
        } else {
            tracing::info!(target: "kaibo::consult", "{msg}");
        }
    }
}

/// A [`ProgressSink`] decorator that remembers the most recent beat while teeing each
/// event to an inner sink. An async `consult` job streams its progress to the caller
/// through `wait` (the inner [`TracingSink`] → notification ring); a caller polling with
/// `get` reads no stream, so the job also holds one of these and `get` echoes
/// [`latest`](Self::latest) inline. Records *and* forwards — the `wait`/`mcp_log` view is
/// unchanged. State is a tiny `Mutex` (last message + beat count); `emit` stays sync and
/// infallible per the [`ProgressSink`] contract.
#[derive(Debug)]
pub struct ProgressLog {
    inner: Arc<dyn ProgressSink>,
    state: Mutex<ProgressState>,
}

#[derive(Debug, Default)]
struct ProgressState {
    /// The most recent event's glanceable one-liner, or `None` before the first beat.
    latest: Option<String>,
    /// How many beats have fired — lets `get` show forward motion ("step 7") even when
    /// two polls land on the same kind of beat.
    steps: u64,
}

impl ProgressLog {
    /// Wrap `inner`, recording each event before forwarding it. Pass [`NullSink`] for a
    /// record-only log with nowhere to tee (what a test or a no-`wait` client uses).
    pub fn new(inner: Arc<dyn ProgressSink>) -> Self {
        Self {
            inner,
            state: Mutex::new(ProgressState::default()),
        }
    }

    /// A record-only log: nothing downstream, just the latest beat for `get` to echo.
    pub fn silent() -> Self {
        Self::new(Arc::new(NullSink))
    }

    /// The most recent beat's one-liner and the running beat count, or `None` if the
    /// phase hasn't emitted yet. `get` renders this as the "currently …" tail on a
    /// still-running job.
    pub fn latest(&self) -> Option<(String, u64)> {
        let s = self.state.lock().expect("progress log mutex poisoned");
        s.latest.clone().map(|msg| (msg, s.steps))
    }
}

impl ProgressSink for ProgressLog {
    fn emit(&self, event: PhaseEvent) {
        // One-liner first (it borrows `event`), then forward the owned event downstream.
        let msg = event.message();
        {
            let mut s = self.state.lock().expect("progress log mutex poisoned");
            s.latest = Some(msg);
            s.steps += 1;
        }
        self.inner.emit(event);
    }
}

/// Does this event clear kaibo's **Warn** bar — "the calling model should see this"? Only
/// the research-limit beat does today (the answer was written early, which changes how a
/// caller reads it); the rest are the Info-level narrative. Split out as a pure predicate
/// so the convention is testable without a `tracing` subscriber (whose capture tests are
/// flaky — see project memory).
fn promotes_to_caller(event: &PhaseEvent) -> bool {
    matches!(event, PhaseEvent::TurnCapReached)
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
        assert!(PhaseEvent::TurnCapReached
            .message()
            .contains("research limit"));
        assert_eq!(
            PhaseEvent::KaishRun {
                script: "grep -rn TODO src".into()
            }
            .message(),
            "running kaish: grep -rn TODO src"
        );
        assert_eq!(
            PhaseEvent::SweepStarted {
                question: "where is the sandbox?".into()
            }
            .message(),
            "exploring: where is the sandbox?"
        );
    }

    #[test]
    fn brief_clips_to_one_line_and_caps_length() {
        // Multi-line script collapses to its first line.
        assert_eq!(brief("cat -n a\ngrep b\nfind c", 80), "cat -n a");
        // Leading whitespace/newlines are trimmed before the first line is taken.
        assert_eq!(brief("\n  grep -n x  \n", 80), "grep -n x");
        // Over the cap → clipped with an ellipsis, never longer than the cap.
        let long = "x".repeat(200);
        let out = brief(&long, 80);
        assert_eq!(
            out.chars().count(),
            80,
            "clip keeps max chars incl. the ellipsis"
        );
        assert!(
            out.ends_with('…'),
            "an over-length clip is marked with an ellipsis"
        );
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

    /// The Warn-bar convention: only `TurnCapReached` promotes to the calling model; the
    /// rest are the Info-level narrative. Pure predicate, so no flaky `tracing` capture.
    #[test]
    fn only_the_research_limit_promotes_to_the_caller() {
        assert!(promotes_to_caller(&PhaseEvent::TurnCapReached));
        for event in [
            PhaseEvent::PhaseStarted { phase: "consult" },
            PhaseEvent::PhaseFinished { phase: "consult" },
            PhaseEvent::SweepStarted {
                question: "q".into(),
            },
            PhaseEvent::SweepFinished,
            PhaseEvent::KaishRun {
                script: "cat -n x".into(),
            },
        ] {
            assert!(
                !promotes_to_caller(&event),
                "{event:?} is Info-narrative, not a caller promotion"
            );
        }
    }

    #[test]
    fn tracing_sink_handles_every_variant_without_panic() {
        // The thin adapter must take every event (levels are verified live / by the pure
        // predicate above, not by a flaky subscriber-capture test).
        let sink = TracingSink;
        sink.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        sink.emit(PhaseEvent::KaishRun {
            script: "grep -rn TODO .".into(),
        });
        sink.emit(PhaseEvent::SweepStarted {
            question: "where?".into(),
        });
        sink.emit(PhaseEvent::SweepFinished);
        sink.emit(PhaseEvent::TurnCapReached);
        sink.emit(PhaseEvent::PhaseFinished { phase: "consult" });
    }

    #[test]
    fn progress_log_starts_empty_then_remembers_the_latest_beat() {
        let log = ProgressLog::silent();
        // Before any beat, there's nothing to echo.
        assert_eq!(log.latest(), None);

        log.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        assert_eq!(log.latest(), Some(("starting consult".to_string(), 1)));

        // A second beat replaces the message and advances the count.
        log.emit(PhaseEvent::SweepStarted {
            question: "where is the sandbox?".into(),
        });
        assert_eq!(
            log.latest(),
            Some(("exploring: where is the sandbox?".to_string(), 2))
        );
    }

    #[test]
    fn progress_log_step_count_advances_on_a_repeated_event_kind() {
        // The step count is the "forward motion" signal `get` shows, so it must advance on
        // *every* beat — including two of the same kind in a row (two `KaishRun`s), where
        // the message alone wouldn't tell a poller anything moved.
        let log = ProgressLog::silent();
        log.emit(PhaseEvent::KaishRun {
            script: "cat -n a.rs".into(),
        });
        assert_eq!(log.latest(), Some(("running kaish: cat -n a.rs".to_string(), 1)));
        log.emit(PhaseEvent::KaishRun {
            script: "grep -rn foo .".into(),
        });
        assert_eq!(
            log.latest(),
            Some(("running kaish: grep -rn foo .".to_string(), 2)),
            "a second beat of the same kind still advances the step count"
        );
    }

    #[test]
    fn progress_log_tees_every_event_to_its_inner_sink() {
        // A counting sink proves the decorator forwards, so the `wait`/mcp_log stream is
        // untouched when a job also records for `get`.
        #[derive(Debug, Default)]
        struct Counter(std::sync::atomic::AtomicUsize);
        impl ProgressSink for Counter {
            fn emit(&self, _event: PhaseEvent) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let counter = Arc::new(Counter::default());
        let log = ProgressLog::new(counter.clone());
        log.emit(PhaseEvent::SweepFinished);
        log.emit(PhaseEvent::PhaseFinished { phase: "consult" });
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        // And it still recorded the last one for `get`.
        assert_eq!(log.latest(), Some(("consult complete".to_string(), 2)));
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
