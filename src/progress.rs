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

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

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
    /// A sweep routed a workspace file's bytes past itself to its report's reader
    /// with the `attach` tool. The one beat that lets an operator actually observe
    /// the pattern the generous `max_attachments` default exists to watch for.
    Attached { path: String },
    /// One model call finished — succeeded or failed — after `elapsed`. `agent` is the
    /// cast role that made it (`"synth"` / `"explorer"`), the axis a slow backend shows
    /// up on: the synth's calls carry the whole transcript and are the ones that stall.
    /// Fires from the completion wrapper, so every phase that runs the tool loop emits
    /// it; the single-shot lanes (`oneshot`, `deliberate`'s direct lane) do not.
    ChatCompleted {
        agent: &'static str,
        elapsed: Duration,
    },
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
            PhaseEvent::Attached { path } => format!("attached {path} to the report"),
            PhaseEvent::ChatCompleted { agent, elapsed } => {
                format!("{agent} chat: {}", secs(*elapsed))
            }
            PhaseEvent::TurnCapReached => "reached research limit, writing the answer".to_string(),
            PhaseEvent::PhaseFinished { phase } => format!("{phase} complete"),
        }
    }
}

/// A duration as seconds for a progress line: one decimal under ten seconds (an
/// explorer call is often `1.5 s`, and `2 s` would hide the difference), whole seconds
/// from there (`107 s`).
fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1} s")
    } else {
        format!("{s:.0} s")
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
/// gave), and the notification ring buffer tees them for `job_wait`.
///
/// Levels follow kaibo's convention — **Warn = "promote to the calling model"**, Info =
/// the watchable narrative — *not* severity:
/// - `KaishRun`, the sweep events, each `ChatCompleted`, and phase start/finish →
///   **Info**: each shell command, model call, and milestone, the user's continuous view.
/// - `TurnCapReached` → **Warn**: the caller should know the research budget ran out and
///   the answer was written early, so it surfaces in the model's `job_wait` drain.
/// - A `ChatCompleted` at or past `slow_chat_after` → **Warn**, worded as the refusal
///   guide asks (what happened, why it matters, what to do): a caller learns a slow
///   backend at the first slow call instead of at the deadline. The threshold lives on
///   this sink because it is an audience decision — who is told — not a phase input;
///   `[defaults] slow_chat_secs` sets it, `0` turns it off.
#[derive(Debug, Clone, Copy)]
pub struct TracingSink {
    slow_chat_after: Option<Duration>,
}

impl TracingSink {
    /// A sink that promotes a chat call at or past `slow_chat_after` to the caller;
    /// `None` never promotes one.
    pub fn new(slow_chat_after: Option<Duration>) -> Self {
        Self { slow_chat_after }
    }
}

impl Default for TracingSink {
    /// The shipped threshold, so a sink built without config behaves like a config-less
    /// server.
    fn default() -> Self {
        Self::new(crate::config::Defaults::default().slow_chat)
    }
}

impl ProgressSink for TracingSink {
    fn emit(&self, event: PhaseEvent) {
        // `event.message()` is the same tidy one-liner sync consult streamed; reuse it so
        // the two channels read identically. The level branch is the only divergence —
        // and the slow-call promotion, which says more than the one-liner because it is
        // read at the moment a caller decides whether to keep waiting.
        if promotes_to_caller(&event, self.slow_chat_after) {
            let msg = match (&event, self.slow_chat_after) {
                (PhaseEvent::ChatCompleted { agent, elapsed }, Some(limit)) => format!(
                    "{agent} chat call took {}, past the {} `slow_chat_secs` mark. The \
                     model is answering slowly. `job_get` shows the running latency; \
                     `job_cancel` and pick another cast if it stays slow.",
                    secs(*elapsed),
                    secs(limit)
                ),
                _ => event.message(),
            };
            tracing::warn!(target: "kaibo::consult", "{msg}");
        } else {
            tracing::info!(target: "kaibo::consult", "{}", event.message());
        }
    }
}

/// A [`ProgressSink`] that renders each [`PhaseEvent`] as one concise line on
/// **stderr** — the CLI front door's liveness channel. stdout carries the answer
/// (so a script can capture it cleanly); progress and logs go to stderr, so a
/// human watching a `kaibo consult` sees the same "watch it work" narrative an MCP
/// client gets over `notifications/progress`, without polluting the captured answer.
///
/// Each line reuses [`PhaseEvent::message`] — the identical glanceable one-liner
/// the MCP wire streams — prefixed `kaibo:` so it reads as tool chatter, not answer
/// text. `emit` stays sync and infallible per the [`ProgressSink`] contract; a
/// failed stderr write is dropped rather than allowed to sink the consult.
#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalSink;

impl ProgressSink for TerminalSink {
    fn emit(&self, event: PhaseEvent) {
        // `writeln!` to a locked stderr handle; ignore a write error — losing a
        // progress beat must never fail the consultation (fire-and-forget contract).
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "kaibo: {}", event.message());
    }
}

/// A [`ProgressSink`] decorator that remembers the most recent beat while teeing each
/// event to an inner sink. An async `consult` job streams its progress to the caller
/// through `job_wait` (the inner [`TracingSink`] → notification ring); a caller polling with
/// `job_get` reads no stream, so the job also holds one of these and `job_get` echoes
/// [`latest`](Self::latest) inline. Records *and* forwards — the `job_wait`/`mcp_log` view is
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
    /// How many beats have fired — lets `job_get` show forward motion ("step 7") even when
    /// two polls land on the same kind of beat.
    steps: u64,
    /// Every model call's duration, per cast role, in the order they finished. A chat
    /// beat records here instead of in `latest`: the "currently" line tracks what the
    /// phase is doing, and this tracks how fast the models answer — two axes a poller
    /// reads side by side. Bounded by the turn caps (a few hundred entries at most).
    chat: BTreeMap<&'static str, Vec<Duration>>,
}

impl ProgressLog {
    /// Wrap `inner`, recording each event before forwarding it. Pass [`NullSink`] for a
    /// record-only log with nowhere to tee (what a test or a no-`job_wait` client uses).
    pub fn new(inner: Arc<dyn ProgressSink>) -> Self {
        Self {
            inner,
            state: Mutex::new(ProgressState::default()),
        }
    }

    /// A record-only log: nothing downstream, just the latest beat for `job_get` to echo.
    pub fn silent() -> Self {
        Self::new(Arc::new(NullSink))
    }

    /// The most recent beat's one-liner and the running beat count, or `None` if the
    /// phase hasn't emitted yet. `job_get` renders this as the "currently …" tail on a
    /// still-running job.
    pub fn latest(&self) -> Option<(String, u64)> {
        let s = self.state.lock().expect("progress log mutex poisoned");
        s.latest.clone().map(|msg| (msg, s.steps))
    }

    /// How fast each role's model is answering, as one clause per role that has made a
    /// call — `synth chat: last 107 s, p50 98 s over 9 calls` — joined with `; `, or
    /// `None` before the first call. `job_get` renders this beside the latest beat, so a
    /// poller can tell a slow backend from a busy one at the first poll.
    pub fn chat_summary(&self) -> Option<String> {
        let s = self.state.lock().expect("progress log mutex poisoned");
        let clauses: Vec<String> = s
            .chat
            .iter()
            .filter(|(_, calls)| !calls.is_empty())
            .map(|(agent, calls)| {
                let last = *calls.last().expect("filtered non-empty");
                let n = calls.len();
                let calls_word = if n == 1 { "call" } else { "calls" };
                format!(
                    "{agent} chat: last {}, p50 {} over {n} {calls_word}",
                    secs(last),
                    secs(median(calls))
                )
            })
            .collect();
        (!clauses.is_empty()).then(|| clauses.join("; "))
    }
}

/// The middle value of `calls` (the upper middle for an even count), so the summary
/// reads as the typical call and one outlier cannot drag it.
fn median(calls: &[Duration]) -> Duration {
    let mut sorted = calls.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

impl ProgressSink for ProgressLog {
    fn emit(&self, event: PhaseEvent) {
        {
            let mut s = self.state.lock().expect("progress log mutex poisoned");
            match &event {
                // A model call's duration is its own axis: it feeds the latency summary
                // and leaves the "currently" line and its step count to the tool beats.
                PhaseEvent::ChatCompleted { agent, elapsed } => {
                    s.chat.entry(agent).or_default().push(*elapsed);
                }
                _ => {
                    s.latest = Some(event.message());
                    s.steps += 1;
                }
            }
        }
        self.inner.emit(event);
    }
}

/// Does this event clear kaibo's **Warn** bar — "the calling model should see this"? The
/// research-limit beat does (the answer was written early, which changes how a caller
/// reads it), and so does a model call at or past `slow_chat_after` (the caller may want
/// to stop waiting); the rest are the Info-level narrative. Split out as a pure predicate
/// so the convention is testable without a `tracing` subscriber (whose capture tests are
/// flaky — see project memory). The threshold is an argument rather than state so the
/// predicate stays pure.
fn promotes_to_caller(event: &PhaseEvent, slow_chat_after: Option<Duration>) -> bool {
    match event {
        PhaseEvent::TurnCapReached => true,
        PhaseEvent::ChatCompleted { elapsed, .. } => {
            slow_chat_after.is_some_and(|limit| *elapsed >= limit)
        }
        _ => false,
    }
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
        // Under ten seconds keeps a decimal (explorer calls live there); from ten on,
        // whole seconds.
        assert_eq!(
            PhaseEvent::ChatCompleted {
                agent: "explorer",
                elapsed: Duration::from_millis(1540)
            }
            .message(),
            "explorer chat: 1.5 s"
        );
        assert_eq!(
            PhaseEvent::ChatCompleted {
                agent: "synth",
                elapsed: Duration::from_millis(107_400)
            }
            .message(),
            "synth chat: 107 s"
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

    /// The Warn-bar convention: `TurnCapReached` and a slow chat call promote to the
    /// calling model; the rest are the Info-level narrative. Pure predicate, so no flaky
    /// `tracing` capture.
    #[test]
    fn the_research_limit_and_a_slow_chat_promote_to_the_caller() {
        let limit = Some(Duration::from_secs(60));
        assert!(promotes_to_caller(&PhaseEvent::TurnCapReached, limit));
        assert!(promotes_to_caller(&PhaseEvent::TurnCapReached, None));
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
            PhaseEvent::Attached { path: "x".into() },
        ] {
            assert!(
                !promotes_to_caller(&event, limit),
                "{event:?} is Info-narrative, not a caller promotion"
            );
        }
    }

    /// A chat call promotes at the mark and past it, never under it, and never when the
    /// operator turned the mark off (`slow_chat_secs = 0` → `None`).
    #[test]
    fn a_chat_call_promotes_only_at_or_past_the_slow_mark() {
        let chat = |ms: u64| PhaseEvent::ChatCompleted {
            agent: "synth",
            elapsed: Duration::from_millis(ms),
        };
        let limit = Some(Duration::from_secs(60));
        assert!(!promotes_to_caller(&chat(59_999), limit));
        assert!(promotes_to_caller(&chat(60_000), limit));
        assert!(promotes_to_caller(&chat(107_000), limit));
        assert!(!promotes_to_caller(&chat(107_000), None), "no mark → never");
    }

    #[test]
    fn tracing_sink_handles_every_variant_without_panic() {
        // The thin adapter must take every event (levels are verified live / by the pure
        // predicate above, not by a flaky subscriber-capture test).
        let sink = TracingSink::default();
        sink.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        sink.emit(PhaseEvent::KaishRun {
            script: "grep -rn TODO .".into(),
        });
        sink.emit(PhaseEvent::SweepStarted {
            question: "where?".into(),
        });
        sink.emit(PhaseEvent::SweepFinished);
        sink.emit(PhaseEvent::TurnCapReached);
        // Both branches of the chat beat: under the mark and past it.
        sink.emit(PhaseEvent::ChatCompleted {
            agent: "explorer",
            elapsed: Duration::from_secs(1),
        });
        sink.emit(PhaseEvent::ChatCompleted {
            agent: "synth",
            elapsed: Duration::from_secs(100_000),
        });
        sink.emit(PhaseEvent::PhaseFinished { phase: "consult" });
    }

    /// The shipped sink carries the shipped mark, so a sink built with no config in hand
    /// promotes the same calls a config-less server would.
    #[test]
    fn the_default_tracing_sink_carries_the_shipped_mark() {
        assert_eq!(
            TracingSink::default().slow_chat_after,
            crate::config::Defaults::default().slow_chat
        );
        assert_eq!(TracingSink::new(None).slow_chat_after, None);
    }

    #[test]
    fn terminal_sink_handles_every_variant_without_panic() {
        // The CLI's stderr sink must take every event; the line content is
        // `PhaseEvent::message` (covered above), so here we only assert it can't panic
        // or block on any variant (the fire-and-forget contract).
        let sink = TerminalSink;
        sink.emit(PhaseEvent::PhaseStarted { phase: "consult" });
        sink.emit(PhaseEvent::KaishRun {
            script: "grep -rn TODO .".into(),
        });
        sink.emit(PhaseEvent::SweepStarted {
            question: "where?".into(),
        });
        sink.emit(PhaseEvent::SweepFinished);
        sink.emit(PhaseEvent::TurnCapReached);
        sink.emit(PhaseEvent::ChatCompleted {
            agent: "synth",
            elapsed: Duration::from_secs(3),
        });
        sink.emit(PhaseEvent::PhaseFinished { phase: "consult" });
    }

    /// A chat beat feeds the latency summary and leaves the "currently" line alone: the
    /// beat count is tool activity, and a poller must not see a finished model call
    /// replace the command the phase is running.
    #[test]
    fn a_chat_beat_feeds_the_summary_and_leaves_the_latest_beat_alone() {
        let log = ProgressLog::silent();
        assert_eq!(log.chat_summary(), None, "nothing before the first call");
        log.emit(PhaseEvent::KaishRun {
            script: "cat -n x".into(),
        });
        log.emit(PhaseEvent::ChatCompleted {
            agent: "synth",
            elapsed: Duration::from_secs(3),
        });
        assert_eq!(
            log.latest(),
            Some(("running kaish: cat -n x".to_string(), 1)),
            "the chat beat neither replaces the latest line nor counts as a step"
        );
        assert_eq!(
            log.chat_summary().as_deref(),
            Some("synth chat: last 3.0 s, p50 3.0 s over 1 call")
        );
    }

    /// The summary reads `last`, the median, and the count per role — roles sorted, so
    /// `explorer` precedes `synth` — and the median is the upper middle of an even count.
    #[test]
    fn chat_summary_reports_last_median_and_count_per_role() {
        let log = ProgressLog::silent();
        for secs in [90, 100, 110, 107] {
            log.emit(PhaseEvent::ChatCompleted {
                agent: "synth",
                elapsed: Duration::from_secs(secs),
            });
        }
        log.emit(PhaseEvent::ChatCompleted {
            agent: "explorer",
            elapsed: Duration::from_millis(1500),
        });
        assert_eq!(
            log.chat_summary().as_deref(),
            Some(
                "explorer chat: last 1.5 s, p50 1.5 s over 1 call; \
                 synth chat: last 107 s, p50 107 s over 4 calls"
            )
        );
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
        // The step count is the "forward motion" signal `job_get` shows, so it must advance on
        // *every* beat — including two of the same kind in a row (two `KaishRun`s), where
        // the message alone wouldn't tell a poller anything moved.
        let log = ProgressLog::silent();
        log.emit(PhaseEvent::KaishRun {
            script: "cat -n a.rs".into(),
        });
        assert_eq!(
            log.latest(),
            Some(("running kaish: cat -n a.rs".to_string(), 1))
        );
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
        // A counting sink proves the decorator forwards, so the `job_wait`/mcp_log stream is
        // untouched when a job also records for `job_get`.
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
        // And it still recorded the last one for `job_get`.
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
