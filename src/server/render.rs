//! Result rendering for the model-driven and async tools — the free-fn cluster that
//! turns a consult / job / batch outcome into a `CallToolResult` or a wire string:
//! consult answers and their provenance footer, failure classification, and the
//! job / batch / wait status views.

use rmcp::model::{CallToolResult, Content, LoggingLevel};
use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::jobs::{JobSnapshot, JobState, JobStore};

/// Assemble the `consult` tool result. The answer is always the text content
/// (unchanged from a bare consult). The explorer's aggregated report — the
/// `explore′` sweeps the driver delegated — rides along as `structured_content`
/// only when the caller set `include_report`, keeping a normal call lean. When
/// requested it is surfaced even if empty: an empty report is the honest signal
/// that the consult read every span itself and delegated no sweep, which is
/// distinct from the caller not asking at all. Pure and offline-testable.
pub(super) fn consult_result(
    answer: String,
    report: String,
    include_report: bool,
) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(answer)]);
    if include_report {
        result.structured_content = Some(json!({ "report": report }));
    }
    result
}

/// How a runtime consultation failure should be framed to the calling agent — derived
/// from the error chain by [`classify_failure`].
#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    /// A transient provider condition (overload / rate-limit / timeout / reset). Worth a
    /// caller-driven manual retry.
    TransientProvider,
    /// A non-transient model/provider error (auth, bad request). Retrying won't help.
    Provider,
    /// A kaibo-*side* failure (e.g. the synth's kaish kernel failed to build) — not the
    /// provider's fault, so we must not say it was.
    Internal,
}

/// Classify a consultation failure from its error chain. This is a **heuristic on the
/// error text**, by necessity: rig collapses the HTTP status into the response *body*
/// (`CompletionError::ProviderError(text)` carries Anthropic's `overloaded_error` JSON, a
/// Gemini `RESOURCE_EXHAUSTED`, etc. — not the number `529`), so we match the providers'
/// transient *vocabulary* rather than a status code. The model loop wraps its errors as
/// `"model loop failed: …"` (`consult.rs`); an error chain lacking that marker came from
/// *before* a model ran (a kaish kernel build inside the toolset factory), so it's a
/// kaibo-side failure, not the provider's.
fn classify_failure(err: &anyhow::Error) -> FailureKind {
    let s = format!("{err:#}").to_lowercase();
    // Our own wall-clock backstop firing (`call_deadline`): the backend stalled past
    // the ceiling and we aborted. Not a kaibo bug and not a model rejection — a
    // transient "no response in time", steered like a provider timeout (retry, raise
    // the deadline, or proceed). Detected before the model-loop gate because the abort
    // happens *around* the loop, so it carries no "model loop failed" marker.
    if s.contains("wall-clock deadline") {
        return FailureKind::TransientProvider;
    }
    let from_model_loop = s.contains("model loop failed") || s.contains("model used all");
    if !from_model_loop {
        return FailureKind::Internal;
    }
    // Transient vocabulary across Anthropic / Gemini / OpenAI / DeepSeek bodies and the
    // transport layer (reqwest timeouts/resets from our own `request_timeout`).
    const TRANSIENT: &[&str] = &[
        "overload",   // Anthropic 529 overloaded_error, Gemini
        "rate limit", // generic
        "rate_limit", // OpenAI/DeepSeek/Anthropic error `type`s
        "ratelimit",
        "resource_exhausted", // Gemini 429
        "too many requests",  // 429 reason phrase
        "timed out",          // reqwest / gateway
        "timeout",
        "connection reset",
        "reset by peer",
        "connection closed",
        "broken pipe",
        "temporarily", // "temporarily unavailable"
        "unavailable", // 503 / Gemini UNAVAILABLE
        "try again",
    ];
    if TRANSIENT.iter().any(|t| s.contains(t)) {
        FailureKind::TransientProvider
    } else {
        FailureKind::Provider
    }
}

/// Surface a *runtime* consultation failure as a **tool-result error** (`is_error =
/// true`) rather than a protocol-level `internal_error`. A consult is an *optional*
/// augmentation: the calling agent should read a clear message and proceed *without* the
/// second opinion — not have its own tool call fail at the JSON-RPC layer. The framing is
/// tailored by [`classify_failure`] so the agent can drive the right next step: a
/// transient overload/timeout invites a manual retry (kaibo does **not** retry on its own
/// — one completion is bounded by the backend's `request_timeout`/`connect_timeout`; see
/// the failure-policy FAQ and `docs/config.md`), a non-transient provider error doesn't,
/// and a kaibo-side failure is named honestly rather than blamed on the provider. Setup
/// errors *before* the model call — unknown cast, an attachment outside the boundary, a
/// missing key — stay `McpError`, since those are the caller's to fix.
pub(super) fn consultation_failed(tool: &str, cast: &str, err: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![Content::text(consultation_failure_text(
        tool, cast, err,
    ))])
}

/// The rendered failure text (detail + classified guidance) for a consultation that
/// errored — the body of [`consultation_failed`], split out so the async path
/// ([`consult_submit`]) can store a ready string in a [`JobState::Failed`] and the
/// unified `job_get` wrap it without re-classifying.
pub(super) fn consultation_failure_text(tool: &str, cast: &str, err: anyhow::Error) -> String {
    let detail = format!("{err:#}");
    let guidance = match classify_failure(&err) {
        FailureKind::TransientProvider => {
            "This looks like a transient provider condition (overload, rate limit, or \
             timeout). kaibo does not retry automatically — you may retry this call, or \
             proceed without the consultation."
        }
        FailureKind::Provider => {
            "The model or its provider rejected the request; retrying is unlikely to help \
             — proceed without the consultation, or check the cast and config."
        }
        FailureKind::Internal => {
            "This is a kaibo-side error (not the provider) — please report it; you can \
             still proceed without the consultation."
        }
    };
    format!("{tool} could not complete (cast `{cast}`): {detail}. {guidance}")
}

/// Render a still-running job's latest progress beat as an inline phrase, or `""` before
/// the first beat lands. Echoes the same one-liner `job_wait` streams (e.g. "exploring: …"),
/// so a caller polling with `job_get` sees forward motion — `step N` advances every beat even
/// when two polls catch the same kind of event. See [`crate::progress::ProgressLog`].
fn running_beat(last_progress: &Option<(String, u64)>) -> String {
    match last_progress {
        Some((msg, steps)) => format!(", currently: {msg} (step {steps})"),
        None => String::new(),
    }
}

/// Render an async consultation job for the unified `job_get`: a status line while it runs,
/// the grounded answer (and optional report) when it's done, the stored failure text on
/// error, or a canceled notice. Mirrors the synchronous `consult` result shape so an
/// agent reads the same thing whether it asked synchronously or collected a job.
pub(super) fn render_job(id: &str, snap: JobSnapshot) -> CallToolResult {
    match snap.state {
        JobState::Running => CallToolResult::success(vec![Content::text(format!(
            "Consultation `{id}` is still running — {} ({}s elapsed){}. No need to wait: go \
             do other work and `job_get` it again later.",
            snap.label,
            snap.age.as_secs(),
            running_beat(&snap.last_progress),
        ))]),
        JobState::Done(result) => {
            let include_report = result.report.is_some();
            consult_result(
                result.answer,
                result.report.unwrap_or_default(),
                include_report,
            )
        }
        JobState::Failed(text) => CallToolResult::error(vec![Content::text(text)]),
        JobState::Canceled => CallToolResult::success(vec![Content::text(format!(
            "Consultation `{id}` was canceled."
        ))]),
    }
}

/// Append a one-line provenance footer naming the cast and the model(s) that
/// produced `answer`. The point is legibility: a caller — a cross-model study most
/// of all — should see *which* model answered without cross-referencing
/// `kaibo://config`, since the answering model is the whole variable. `roles` is the
/// labelled models for this tool (one for `oneshot`, explorer+synth for `consult`).
/// Pure and offline-testable.
/// Which kind of async work a handle addresses. A batch handle carries a `/`
/// (`backend/provider-id`); a consult job id (`job-N`) never does, and a backend name
/// carries no `/` either (enforced at config load), so the presence of a `/` is an
/// unambiguous batch-vs-consult discriminator for the unified `job_get`/`job_cancel` verbs.
pub(super) fn is_batch_handle(handle: &str) -> bool {
    handle.contains('/')
}

/// Map a `job_wait` `level` string to a [`mcp_log::rank`] floor. `warn` (the default) is
/// kaibo's "the calling model should see this" bar — salience, not severity.
pub(super) fn wait_level_floor(level: Option<&str>) -> Result<u8, McpError> {
    let l = match level.unwrap_or("warn").to_ascii_lowercase().as_str() {
        "debug" => LoggingLevel::Debug,
        "info" => LoggingLevel::Info,
        "warn" | "warning" => LoggingLevel::Warning,
        "error" => LoggingLevel::Error,
        other => {
            return Err(McpError::invalid_params(
                format!("level must be one of debug|info|warn|error, got {other:?}"),
                None,
            ));
        }
    };
    Ok(crate::mcp_log::rank(l))
}

/// A short, readable tag for a record's level in `job_wait` output.
pub(super) fn wait_level_label(level: LoggingLevel) -> &'static str {
    match level {
        LoggingLevel::Debug => "debug",
        LoggingLevel::Info => "info",
        LoggingLevel::Notice => "note",
        LoggingLevel::Warning => "warn",
        LoggingLevel::Error
        | LoggingLevel::Critical
        | LoggingLevel::Alert
        | LoggingLevel::Emergency => "error",
    }
}

/// One-line status for a batch poll, for the `job_wait` footer — not the full results
/// (`job_get` the handle for those).
pub(super) fn batch_poll_brief(poll: &crate::batch::BatchPoll) -> String {
    match poll {
        crate::batch::BatchPoll::Pending { completed, total } => {
            format!("running, {completed}/{total} done")
        }
        crate::batch::BatchPoll::Cancelling => "canceling".to_string(),
        crate::batch::BatchPoll::Done(answers) => {
            format!("complete — {} result(s); `job_get` it", answers.len())
        }
        crate::batch::BatchPoll::Failed { state, .. } => format!("ended ({state})"),
    }
}

/// Render a `job_wait` result: the drained activity records (each tagged by level), any
/// gentle batch-poll lines, and a footer naming the consult jobs still running.
pub(super) fn render_wait(
    records: &[crate::mcp_log::LogRecord],
    batch_lines: &[String],
    jobs: &JobStore,
    timeout: std::time::Duration,
) -> String {
    let mut s = String::new();
    if records.is_empty() && batch_lines.is_empty() {
        s.push_str(&format!("Nothing new in {}s.", timeout.as_secs()));
    } else {
        for r in records {
            s.push_str(&format!("[{}] {}\n", wait_level_label(r.level), r.message));
        }
        for b in batch_lines {
            s.push_str(b);
            s.push('\n');
        }
    }
    let running: Vec<String> = jobs
        .list()
        .into_iter()
        .filter(|(_, snap)| matches!(snap.state, JobState::Running))
        .map(|(id, _)| id)
        .collect();
    s.push('\n');
    if running.is_empty() {
        s.push_str("No consult jobs still running.");
    } else {
        s.push_str(&format!("Still running: {}.", running.join(", ")));
    }
    s.push_str(
        " `job_get <handle>` to collect a finished one, `job_list` for the full picture, or \
         `job_wait` again to keep parking.",
    );
    s
}

/// The default `job_list` recency window: 24h. A provider's offline SLA is ≤24h, so an
/// older batch is done and still collectible by its handle — trimming it saves the
/// caller tokens without losing anything actionable.
pub(super) const BATCH_RECENCY_WINDOW_SECS: i64 = 24 * 3600;

/// Unix epoch seconds now, for the `job_list` recency window — read once per `job_list` call and
/// passed into [`batch_within_window`], not re-read per item. A pre-epoch system clock
/// (impossible in practice) reads as 0, which keeps everything: fail-open, never hide a
/// batch on a clock glitch. From `SystemTime`, so chrono's `clock` feature stays off.
pub(super) fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Is this batch within `window_secs` of `now_epoch`? The `job_list` recency filter, with
/// `now` injected so the keep/drop boundary is testable without the clock and the clock
/// is read once per call rather than per item. A batch kaibo can't date (no or
/// unparseable `created_at`) is kept, not silently hidden — losing sight of a batch is
/// worse than an extra line; a future timestamp (clock skew) yields a negative age, still
/// within the window, so also kept.
pub(super) fn batch_within_window(
    it: &crate::batch::BatchListItem,
    now_epoch: i64,
    window_secs: i64,
) -> bool {
    match it
        .created_at
        .as_deref()
        .and_then(crate::batch::rfc3339_to_epoch)
    {
        Some(created) => now_epoch - created <= window_secs,
        None => true,
    }
}

/// Render the consult-jobs section of `job_list`: the in-memory async consultations this
/// session, newest-first, each with its handle and a one-line state. Empty is itself
/// informative ("none"), so the section always renders.
pub(super) fn render_jobs_section(jobs: &[(String, JobSnapshot)]) -> String {
    if jobs.is_empty() {
        return "Consult jobs (this session): none.".to_string();
    }
    let mut s = String::from("Consult jobs (this session), newest first:");
    for (id, snap) in jobs {
        let state = match &snap.state {
            JobState::Running => {
                format!(
                    "running, {}s{}",
                    snap.age.as_secs(),
                    running_beat(&snap.last_progress)
                )
            }
            JobState::Done(_) => "done — `job_get` it for the answer".to_string(),
            JobState::Failed(_) => "failed — `job_get` it for the reason".to_string(),
            JobState::Canceled => "canceled".to_string(),
        };
        s.push_str(&format!("\n  {id} — {} [{state}]", snap.label));
    }
    s.push_str("\n\nCollect one with `job_get <job-id>`; stop one with `job_cancel <job-id>`.");
    s
}

/// Split a batch handle (`"backend/provider-id"`) the way `batch_submit` minted it.
/// Splitting on the *first* `/` is unambiguous because a backend name carries no `/`
/// (enforced at config load) — so the provider id keeps any slashes of its own (an
/// Anthropic id is `msgbatch_…`; a Gemini id is `batches/<id>`). A malformed handle is a
/// loud parameter error — the caller pasted something that wasn't a kaibo batch id.
pub(super) fn parse_batch_handle(handle: &str) -> Result<(&str, &str), McpError> {
    handle
        .split_once('/')
        .filter(|(b, id)| !b.is_empty() && !id.is_empty())
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "batch id {handle:?} must be \"backend/provider-id\" — pass the handle \
                     kaibo returned from batch_submit"
                ),
                None,
            )
        })
}

pub(super) fn with_provenance(answer: String, cast: &str, roles: &[(&str, &str)]) -> String {
    let models = roles
        .iter()
        .map(|(label, model)| format!("{label} `{model}`"))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{answer}\n\n———\nkaibo · cast `{cast}` · {models}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_level_floor_parses_the_salience_words_and_rejects_junk() {
        use crate::mcp_log::rank;
        // Default is the Warn bar — "the calling model should see this".
        assert_eq!(wait_level_floor(None).unwrap(), rank(LoggingLevel::Warning));
        assert_eq!(
            wait_level_floor(Some("info")).unwrap(),
            rank(LoggingLevel::Info)
        );
        assert_eq!(
            wait_level_floor(Some("WARN")).unwrap(),
            rank(LoggingLevel::Warning)
        );
        assert_eq!(
            wait_level_floor(Some("error")).unwrap(),
            rank(LoggingLevel::Error)
        );
        assert!(
            wait_level_floor(Some("loud")).is_err(),
            "an unknown level is a loud error, not a silent default"
        );
    }

    #[test]
    fn render_wait_tags_records_and_footers_running_jobs() {
        let jobs = JobStore::new(std::num::NonZeroUsize::new(4).unwrap());
        let recs = vec![crate::mcp_log::LogRecord {
            level: LoggingLevel::Warning,
            target: "kaibo::jobs".into(),
            message: "async job finished — collect it with `job_get`".into(),
            fields: serde_json::Map::new(),
        }];
        let out = render_wait(&recs, &[], &jobs, std::time::Duration::from_secs(60));
        assert!(out.contains("[warn]"), "tags the record's level: {out}");
        assert!(
            out.contains("async job finished"),
            "carries the message: {out}"
        );
        assert!(
            out.contains("No consult jobs still running"),
            "footer reports none running: {out}"
        );

        // No records, no batch lines → the clean empty line, naming the wait length.
        let empty = render_wait(&[], &[], &jobs, std::time::Duration::from_secs(30));
        assert!(empty.contains("Nothing new in 30s"), "clean empty: {empty}");
    }

    #[test]
    fn running_beat_renders_a_phrase_only_when_a_beat_exists() {
        assert_eq!(running_beat(&None), "");
        assert_eq!(
            running_beat(&Some(("exploring: the sandbox".to_string(), 3))),
            ", currently: exploring: the sandbox (step 3)"
        );
    }

    #[test]
    fn render_job_echoes_the_latest_beat_on_a_running_job() {
        let text = |r: CallToolResult| {
            r.content
                .into_iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // A running job with a beat shows it inline; one without stays a bare status line.
        let with_beat = render_job(
            "job-1",
            JobSnapshot {
                state: JobState::Running,
                label: "cast `x`".into(),
                age: std::time::Duration::from_secs(12),
                last_progress: Some(("exploring: where?".to_string(), 2)),
            },
        );
        let out = text(with_beat);
        assert!(out.contains("still running"), "status line: {out}");
        assert!(
            out.contains("currently: exploring: where? (step 2)"),
            "echoes the latest beat: {out}"
        );

        let no_beat = render_job(
            "job-2",
            JobSnapshot {
                state: JobState::Running,
                label: "cast `x`".into(),
                age: std::time::Duration::from_secs(1),
                last_progress: None,
            },
        );
        assert!(
            !text(no_beat).contains("currently:"),
            "no beat yet → no 'currently' phrase"
        );
    }

    fn batch_item(created_at: Option<&str>) -> crate::batch::BatchListItem {
        crate::batch::BatchListItem {
            provider_id: "id".into(),
            status: "ended".into(),
            completed: 1,
            total: 1,
            created_at: created_at.map(str::to_string),
        }
    }

    /// The `job_list` recency filter's keep/drop boundary, with `now` pinned so it doesn't
    /// depend on the wall clock. `now` = 2026-06-24T18:00:00Z (epoch 1782756000).
    #[test]
    fn batch_recency_window_keeps_recent_and_undateable_drops_old() {
        let now = crate::batch::rfc3339_to_epoch("2026-06-24T18:00:00Z").unwrap();
        let window = 24 * 3600;

        // 2h old → kept.
        assert!(batch_within_window(
            &batch_item(Some("2026-06-24T16:00:00Z")),
            now,
            window
        ));
        // 30h old → dropped.
        assert!(!batch_within_window(
            &batch_item(Some("2026-06-23T12:00:00Z")),
            now,
            window
        ));
        // Exactly at the 24h edge → kept (boundary is inclusive).
        assert!(batch_within_window(
            &batch_item(Some("2026-06-23T18:00:00Z")),
            now,
            window
        ));
        // Undateable (no timestamp, or garbage) → kept, never silently hidden.
        assert!(batch_within_window(&batch_item(None), now, window));
        assert!(batch_within_window(
            &batch_item(Some("whenever")),
            now,
            window
        ));
        // Future timestamp (clock skew) → kept.
        assert!(batch_within_window(
            &batch_item(Some("2026-06-25T00:00:00Z")),
            now,
            window
        ));
    }

    /// The text channel of a result (the answer). Panics if it isn't a single
    /// text block, which is the only shape `consult_result` produces.
    fn answer_text(result: &CallToolResult) -> String {
        assert_eq!(
            result.content.len(),
            1,
            "consult result is a single text block"
        );
        result.content[0]
            .as_text()
            .expect("consult answer is text content")
            .text
            .clone()
    }

    /// A runtime consultation failure surfaces as a **tool-result error** (`is_error =
    /// true`) carrying the detail — not a protocol-level `internal_error` — so the calling
    /// agent reads "the consult failed, here's why" and proceeds without the second
    /// opinion. The message names the tool and cast and preserves the underlying chain.
    #[test]
    fn consultation_failed_is_a_tool_error_carrying_the_detail() {
        let err = anyhow::anyhow!("model loop failed: ProviderError: overloaded_error");
        let result = consultation_failed("consult", "deepseek", err);
        assert_eq!(
            result.is_error,
            Some(true),
            "a provider failure is a tool-result error, not a success"
        );
        let text = answer_text(&result);
        assert!(text.contains("consult"), "names the tool: {text}");
        assert!(text.contains("deepseek"), "names the cast: {text}");
        assert!(
            text.contains("overloaded_error"),
            "preserves the underlying detail so the host can decide: {text}"
        );
    }

    /// A *transient* provider condition (overload / rate-limit / timeout / reset) is
    /// classified as retryable, so the message invites the calling agent to drive a manual
    /// retry. We match the providers' transient *vocabulary*, not a status number: rig
    /// collapses the HTTP status into the response *body* (`ProviderError(text)`), so the
    /// numeric code isn't reliably present.
    #[test]
    fn transient_provider_failure_suggests_a_manual_retry() {
        for body in [
            "model loop failed: ProviderError: {\"type\":\"overloaded_error\"}",
            "model loop failed: ProviderError: rate_limit_error",
            "model loop failed: HttpError: error sending request: operation timed out",
            "model loop failed: HttpError: connection reset by peer",
            "model loop failed: ProviderError: RESOURCE_EXHAUSTED",
        ] {
            let result = consultation_failed("consult", "gemini", anyhow::anyhow!(body));
            let text = answer_text(&result).to_lowercase();
            assert!(
                text.contains("retry"),
                "a transient failure should invite a manual retry: {body} -> {text}"
            );
        }
    }

    /// A *non-transient* provider error (auth / bad request) does not invite a retry —
    /// retrying won't help — but is still a clean tool-result error.
    #[test]
    fn non_transient_provider_failure_does_not_suggest_retry() {
        let err = anyhow::anyhow!("model loop failed: ProviderError: invalid_request_error");
        let text = answer_text(&consultation_failed("consult", "anthropic", err));
        assert!(
            !text.to_lowercase().contains("you may retry")
                && !text.to_lowercase().contains("retry this call"),
            "a non-transient error must not invite a retry: {text}"
        );
    }

    /// A kaibo-*side* failure (a kaish kernel build, not the model loop) must not be
    /// blamed on the provider — the message names it as a kaibo internal error. (DeepSeek
    /// review, 2026-06-23: the synth's kernel spawns inside the consult error shadow, so a
    /// spawn failure would otherwise read as "the provider failed, proceed without it".)
    #[test]
    fn internal_failure_is_not_blamed_on_the_provider() {
        let err = anyhow::anyhow!("failed to build read-only kaish kernel: out of memory");
        let text = answer_text(&consultation_failed("consult", "deepseek", err));
        let lower = text.to_lowercase();
        assert!(
            lower.contains("kaibo"),
            "a kaibo-side failure is named as such, not the provider's fault: {text}"
        );
        assert!(
            !lower.contains("provider failed") && !lower.contains("provider rejected"),
            "must not claim the provider failed: {text}"
        );
    }

    /// The `call_deadline` backstop firing is a *transient* condition, not a kaibo bug:
    /// a stalled backend the wall-clock timer aborted. The guidance must invite a retry
    /// (or proceed) and must not blame kaibo — the failure mode that stranded a real
    /// consult ~17h (2026-07-02) before this backstop existed.
    #[test]
    fn wall_clock_deadline_is_transient_not_a_kaibo_bug() {
        let err = anyhow::anyhow!(
            "consult loop: consult exceeded its 3600s wall-clock deadline — a backend or \
             model stopped responding."
        );
        let text = answer_text(&consultation_failed("consult", "zorak", err));
        let lower = text.to_lowercase();
        assert!(
            lower.contains("retry"),
            "a deadline abort should invite a manual retry / proceed: {text}"
        );
        assert!(
            !lower.contains("kaibo-side error") && !lower.contains("please report it"),
            "a stalled backend is not a kaibo bug to report: {text}"
        );
    }

    /// Provenance footer: the answer keeps its text, and the cast plus every labelled
    /// model is appended so a caller (a cross-model study most of all) sees which model
    /// produced the answer without cross-referencing `kaibo://config`.
    #[test]
    fn provenance_footer_names_the_cast_and_every_model() {
        let out = with_provenance(
            "the answer".into(),
            "gemini",
            &[
                ("explorer", "gemini-flash-lite-latest"),
                ("synth", "gemini-3.5-flash"),
            ],
        );
        assert!(
            out.starts_with("the answer"),
            "the answer is preserved: {out}"
        );
        assert!(out.contains("cast `gemini`"), "names the cast: {out}");
        assert!(
            out.contains("explorer `gemini-flash-lite-latest`"),
            "names the explorer model: {out}"
        );
        assert!(
            out.contains("synth `gemini-3.5-flash`"),
            "names the synth model: {out}"
        );

        // The oneshot shape: a single labelled model.
        let one = with_provenance("x".into(), "deepseek", &[("model", "deepseek-v4-pro")]);
        assert!(
            one.contains("cast `deepseek` · model `deepseek-v4-pro`"),
            "{one}"
        );
    }

    /// Default path: no report requested ⇒ the answer is the whole result and no
    /// structured content rides along (a lean call, byte-for-byte its pre-flag shape).
    #[test]
    fn consult_result_omits_report_unless_requested() {
        let result = consult_result("the answer".into(), "FILE:1 evidence".into(), false);
        assert_eq!(answer_text(&result), "the answer");
        assert!(
            result.structured_content.is_none(),
            "report must not leak into a default call: {:?}",
            result.structured_content
        );
    }

    /// Opt-in: the report is surfaced as structured_content under `report`, while the
    /// answer stays the text channel — the report rides *separately*, not duplicated
    /// into the answer the model reads.
    #[test]
    fn consult_result_attaches_report_when_requested() {
        let result = consult_result("ans".into(), "src/x.rs:1 the snippet".into(), true);
        assert_eq!(answer_text(&result), "ans", "answer stays the text channel");
        assert!(
            !answer_text(&result).contains("the snippet"),
            "report must not be folded into the answer text"
        );
        let sc = result.structured_content.expect("report was requested");
        assert_eq!(
            sc["report"], "src/x.rs:1 the snippet",
            "report rides under `report`"
        );
    }

    /// Opt-in with an empty report (the consult delegated no sweep): still surfaced.
    /// Emptiness is the signal — present-but-empty means "asked, no sweep happened",
    /// which a caller must be able to tell apart from "never asked" (None).
    #[test]
    fn consult_result_surfaces_empty_report_when_requested() {
        let result = consult_result("ans".into(), String::new(), true);
        let sc = result
            .structured_content
            .expect("requested even when empty");
        assert_eq!(sc["report"], "", "an empty report is surfaced honestly");
    }
}
