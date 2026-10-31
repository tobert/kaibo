//! Two retries when the request was fine and the provider was not: one when it could
//! not parse what the model generated, one when it was rate-limited or overloaded.
//!
//! **Where each retry lives (rig 0.46).** Up to rig-core 0.42 both retries wrapped the
//! `CompletionModel`, underneath the agent loop. 0.46 erases a model to a
//! `DynModel<Completion>` that kaibo cannot wrap, so the two classes moved to the seam
//! each one actually belongs to:
//!
//! - **Transient transport** is an HTTP fact (a status code), so [`RetryingHttp`] wraps
//!   the `HttpClientExt` kaibo hands rig. It sits under the loop exactly as the model
//!   wrapper did: the request still holds the whole transcript, and the agent never
//!   learns a send failed. It also sees the failed reply's headers, so a provider's
//!   `Retry-After` now steers the wait, which the old model-level wrapper could not read.
//! - **Malformed generation** is a *model turn* fact (the provider answered, the
//!   generation is unusable), so [`MalformedGenerationHook`] retries it from the agent's
//!   `on_model_turn_finished`, and [`retry_malformed`] does the same around a single-shot
//!   call. A hook retry spends one of the run's model-call turns; the old wrapper spent
//!   none. That is the one behavior this move changes.
//!
//! **Malformed generation.** A failure class that looks like a hard error and is
//! really a coin that landed wrong: the model fumbled a single tool call, the
//! provider refused to shape the response, and the request that produced it is still
//! perfectly good. Gemini names it `finishReason: MALFORMED_FUNCTION_CALL`. Observed
//! live: a Gemini Flash explorer died on one empty function call twenty turns into a
//! `deliberate`, and the whole investigation went with it.
//!
//! **Transient transport failure.** A 429 (every provider's rate limit), a
//! 500/502/503 (the generic "backend down or overloaded" family), or a 529
//! (Anthropic's own `overloaded_error`) means the same thing at the transport layer:
//! the request was fine, the provider just was not ready for it right now. Observed
//! live: two `deliberate` calls died in the DOSSIER phase on `429 tokens per min
//! (TPM): Limit 200000` from an explorer that had only attached three files, and
//! kaibo neither backed off nor retried — the whole investigation was lost with them.
//!
//! **Why this retries underneath the loop instead of catching the error in it.**
//! rig-agent yields a completion failure straight out of its driver, and the
//! `PromptError::Provider` it becomes carries **no `chat_history`** — unlike
//! `MaxTurns` and `Cancelled`, which hand the transcript back and are exactly why
//! kaibo can recover from a turn cap or a `view_image` break. So by the time the failure
//! reaches [`crate::consult::run_phase`] the turns are gone, and a retry there could only
//! mean re-running the phase from the first prompt. This is the only code in kaibo that
//! sends a provider request twice.
//!
//! **What is retried.** [`MALFORMED_RETRIES`] further attempts for a malformed
//! generation and [`TRANSIENT_RETRIES`] for a transient transport failure, each counted
//! independently. A malformed generation is sent again at once: the provider is not busy,
//! one generation came out wrong, and the next sampling is a fresh chance. A transient
//! failure waits first, because sending it again immediately would land in the same queue
//! that just refused it.
//!
//! **Malformed-generation detection is a heuristic on text**, by the same necessity as
//! `server/render.rs::classify_failure`: the reason arrives as a finish-reason spelling or
//! inside a formatted message, so [`is_malformed_generation`] matches the spellings
//! providers ship. Two neighbors were considered and deliberately left out. The
//! OpenAI-family "Response did not contain a valid message or tool call" also means
//! "unparseable generation", but nothing yet says whether it is a fumble or a refusal, and
//! a wrong guess spends three calls on every refusal. Gemini's `MissingThoughtSignature`
//! is worse: if it fires because a replayed history dropped a signature, it fires on every
//! attempt. Add either one on evidence from a live probe.
//!
//! **Transient classification reads the status, not the text.**
//! [`is_transient_status`] matches only the specific codes in
//! [`TRANSIENT_TRANSPORT_STATUSES`] — never a bare `!is_success()`, because a provider can
//! ship an error envelope with a **2xx** status, and retrying every non-2xx or every status
//! at all would resend requests that were rejected on purpose. [`RetryingHttp`] only sees a
//! status on a failed reply, so a 2xx never reaches it.
//!
//! **Waiting is exponential backoff with full jitter**, plus a best-effort assist
//! from the provider's own words. [`TRANSIENT_BACKOFF_FLOOR`],
//! [`TRANSIENT_BACKOFF_FACTOR`], and [`TRANSIENT_BACKOFF_CAP`] set the schedule; full
//! jitter (a uniform random wait between zero and each attempt's ceiling, the AWS
//! formula) is deliberate because kaibo's own bursts come from several arms hitting one
//! backend's rate limit at the same moment. The wait is whichever is longest of the
//! jittered backoff, the `Retry-After` header, and the body's own delay
//! ([`delay_hint_seconds`], OpenAI's "…Please try again in 20s."), still capped. One
//! `tracing::warn!` per wait names the status, the attempt, and the delay.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rig_agent::agent::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::{AssistantContent, FinishReason};
use rig_core::http_client::{self, HttpClientExt, LazyBody, Request, Response};

/// Attempts a turn gets *after* the first, when the provider could not parse the
/// generation. Two, so a turn costs at most three requests: one glitch is the observed
/// shape, a second in a row is unlucky, and a third says the request itself is the
/// problem and the phase should fail with it.
pub const MALFORMED_RETRIES: usize = 2;

/// The spellings a provider uses when it could not parse the model's tool call or the
/// response around it, lowercased:
///
/// - `malformedfunctioncall` / `malformedresponse` — the Debug spelling of a Gemini
///   finish reason as it appears in a formatted error.
/// - `malformed_function_call` / `malformed_response` — Gemini's own
///   `SCREAMING_SNAKE_CASE` wire values, which rig 0.46 carries verbatim as
///   `FinishReason::Other` and a gateway may forward in text.
/// - `malformed function call` — Gemini's `finishMessage` prose ("malformed function
///   call: default_api"), which a gateway may forward without the reason beside it.
const MALFORMED_MARKERS: &[&str] = &[
    "malformedfunctioncall",
    "malformed_function_call",
    "malformed function call",
    "malformedresponse",
    "malformed_response",
];

/// True when a finish reason or an error text says the provider could not parse what the
/// model generated. Shared with `server/render.rs::classify_failure` so one vocabulary
/// decides both the retry and the advice a caller finally reads — a marker added here
/// reaches both.
pub fn is_malformed_generation(error_text: &str) -> bool {
    let text = error_text.to_lowercase();
    MALFORMED_MARKERS.iter().any(|marker| text.contains(marker))
}

/// Further attempts a request gets after the first, when the provider reports a
/// transient transport failure. Four: a burst can cost up to five requests to ride
/// out, and the backoff schedule below already spends up to a minute doing it — more
/// than that turns one busy provider into a stalled phase.
pub const TRANSIENT_RETRIES: usize = 4;

/// The first wait's ceiling, before jitter. One second: fast enough that a lone 429
/// costs a blink, and the base [`TRANSIENT_BACKOFF_FACTOR`] doubles from here.
pub const TRANSIENT_BACKOFF_FLOOR: Duration = Duration::from_secs(1);

/// The multiplier between one wait's ceiling and the next: 1s, 2s, 4s, 8s, doubling
/// until [`TRANSIENT_BACKOFF_CAP`] takes over.
pub const TRANSIENT_BACKOFF_FACTOR: u32 = 2;

/// The most any single wait runs, jitter, `Retry-After`, and delay hint all included.
/// Each is clamped here, so a provider asking for "600s" is capped the same as an
/// unbounded exponential term would be.
pub const TRANSIENT_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// HTTP statuses that mean "the provider was not ready for this request right now",
/// not "this request is wrong":
///
/// - `429 Too Many Requests` — every provider's rate-limit status.
/// - `500` / `502` / `503` — the generic "the backend is down or overloaded" family,
///   seen from OpenAI-compatible gateways.
/// - `529` — Anthropic's own "Overloaded" status, outside the standard registry.
const TRANSIENT_TRANSPORT_STATUSES: &[u16] = &[429, 500, 502, 503, 529];

/// True when an HTTP status means the provider is rate-limiting or overloaded right now,
/// not refusing the request. A rejected request (400/401/403/404/422) and everything
/// else this does not name keep the default: one attempt, error up.
pub fn is_transient_status(status: u16) -> bool {
    TRANSIENT_TRANSPORT_STATUSES.contains(&status)
}

/// Best-effort seconds parsed from a provider's error body when it embeds its own
/// retry delay — the OpenAI 429 phrasing kaibo has seen: "Rate limit reached … Please
/// try again in 20s." and "… try again in 1.2s". Returns `None` on anything that
/// doesn't match this shape (a different phrasing, no delay at all, a value that
/// fails to parse as a number) — every `None` path falls back to the computed
/// backoff at the call site, never a fabricated wait.
pub fn delay_hint_seconds(body: &str) -> Option<f64> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)try again in\s+([0-9]+(?:\.[0-9]+)?)s\b")
            .expect("delay-hint pattern is a fixed, valid regex")
    });
    let seconds = pattern
        .captures(body)?
        .get(1)?
        .as_str()
        .parse::<f64>()
        .ok()?;
    seconds.is_finite().then_some(seconds)
}

/// The seconds a `Retry-After` header names, when it is the whole-seconds form. The
/// HTTP-date form is not parsed: a provider that sends one gets the computed backoff.
fn retry_after_seconds(headers: &http::HeaderMap) -> Option<f64> {
    let seconds = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds)
}

/// This attempt's backoff ceiling before jitter: `TRANSIENT_BACKOFF_FLOOR *
/// TRANSIENT_BACKOFF_FACTOR^(attempt - 1)`, capped at `TRANSIENT_BACKOFF_CAP`.
/// `attempt` is 1 for the first wait after the original request.
fn backoff_ceiling(attempt: u32) -> Duration {
    let scaled = TRANSIENT_BACKOFF_FLOOR.as_secs_f64()
        * (TRANSIENT_BACKOFF_FACTOR as f64).powi(attempt as i32 - 1);
    Duration::from_secs_f64(scaled).min(TRANSIENT_BACKOFF_CAP)
}

/// The wait before the `attempt`th retry of a transient transport failure: exponential
/// backoff with full jitter — a uniform random duration between zero and this
/// attempt's [`backoff_ceiling`] — raised to the provider's own `Retry-After` header or
/// body delay hint ([`delay_hint_seconds`]) when one is longer, and capped at
/// [`TRANSIENT_BACKOFF_CAP`] either way.
fn transient_wait(attempt: u32, headers: Option<&http::HeaderMap>, body: Option<&str>) -> Duration {
    let ceiling = backoff_ceiling(attempt);
    let backoff = Duration::from_secs_f64(rand::random_range(0.0..=ceiling.as_secs_f64()));
    let asked = [
        headers.and_then(retry_after_seconds),
        body.and_then(delay_hint_seconds).filter(|s| *s >= 0.0),
    ]
    .into_iter()
    .flatten()
    .fold(0.0_f64, f64::max);
    // Cap the number, then convert: a hint can be finite yet larger than a `Duration`
    // holds (twenty digits of seconds), and `from_secs_f64` panics on overflow — so a
    // `.min` on the converted value would run one step too late.
    let seconds = asked
        .max(backoff.as_secs_f64())
        .min(TRANSIENT_BACKOFF_CAP.as_secs_f64());
    Duration::from_secs_f64(seconds)
}

/// An [`HttpClientExt`] that sends a request again, up to [`TRANSIENT_RETRIES`] times,
/// when the provider answers with a transient status ([`is_transient_status`]) — and is
/// otherwise the inner client exactly.
///
/// Transparent on every other path: a success, a rejected status, a transport error
/// with no reply, a multipart upload, and a stream are the inner client's own.
#[derive(Clone, Debug)]
pub struct RetryingHttp<H> {
    inner: H,
    /// The model id, for the warn event — a count of these per model is the whole
    /// point of making the retry observable.
    model: Arc<str>,
}

impl<H> RetryingHttp<H> {
    /// Wrap `inner` so a transient transport failure waits and is sent again.
    pub fn new(inner: H, model: &str) -> Self {
        Self {
            inner,
            model: Arc::from(model),
        }
    }
}

impl<H: HttpClientExt + Clone + 'static> HttpClientExt for RetryingHttp<H> {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let inner = self.inner.clone();
        let model = self.model.clone();
        // Hold the request as bytes so every attempt can rebuild it identically.
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        async move {
            let mut attempt = 0u32;
            loop {
                let again = Request::from_parts(parts.clone(), body.clone());
                let error = match inner.send::<Bytes, U>(again).await {
                    Ok(response) => return Ok(response),
                    Err(error) => error,
                };
                let status = error.non_success_status().map(|s| s.as_u16());
                let transient = status.is_some_and(is_transient_status);
                if !transient || attempt as usize >= TRANSIENT_RETRIES {
                    // Not a transient status, or the allowance is spent: the phase's
                    // failure now, carrying the provider's own words.
                    return Err(error);
                }
                attempt += 1;
                let wait =
                    transient_wait(attempt, error.non_success_headers(), error.non_success_body());
                tracing::warn!(
                    model = %model,
                    status,
                    attempt,
                    retries = TRANSIENT_RETRIES,
                    delay_ms = wait.as_millis() as u64,
                    %error,
                    "the provider reported a transient transport failure — waiting, \
                     then sending the same request again"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }

    // Not retried: kaibo posts no multipart form on a completion, and its tool loop
    // never streams. They forward untouched so the wrapper stays a drop-in for the
    // whole trait.
    fn send_multipart<U>(
        &self,
        req: Request<http_client::MultipartForm>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.inner.send_multipart::<U>(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<http_client::StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        self.inner.send_streaming(req)
    }
}

/// Whether a finish reason and its turn mark a malformed generation worth sending
/// again: rig carries the provider's own spelling as [`FinishReason::Other`], and a turn
/// that holds a tool call is left alone (rig refuses to retry one, so history never
/// holds an unanswered call).
fn malformed_turn(finish: Option<&FinishReason>, content: &[AssistantContent]) -> bool {
    matches!(finish, Some(FinishReason::Other(reason)) if is_malformed_generation(reason))
        && !content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)))
}

/// Sends a turn again when the provider ended it with a malformed-generation finish
/// reason ([`is_malformed_generation`]), up to [`MALFORMED_RETRIES`] times per run.
///
/// One instance per run: the counter is the run's allowance. A retry reuses the same
/// prompt and history, so the next attempt is a fresh sampling of the request that just
/// failed. Every other turn is accepted untouched.
#[derive(Clone, Debug)]
pub struct MalformedGenerationHook {
    model: Arc<str>,
    spent: Arc<AtomicUsize>,
}

impl MalformedGenerationHook {
    /// A hook for one run of `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: Arc::from(model),
            spent: Arc::default(),
        }
    }
}

impl AgentHook for MalformedGenerationHook {
    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if !malformed_turn(event.finish_reason, event.content) {
            return ModelTurnAction::continue_run();
        }
        let attempt = self.spent.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt > MALFORMED_RETRIES {
            return ModelTurnAction::continue_run();
        }
        tracing::warn!(
            model = %self.model,
            attempt,
            retries = MALFORMED_RETRIES,
            reason = ?event.finish_reason,
            "the provider could not parse this turn's tool call — sending the same request again"
        );
        ModelTurnAction::repeat()
    }
}

/// Run one single-shot completion through `call`, sending it again up to
/// [`MALFORMED_RETRIES`] times when the error text says the provider could not parse the
/// generation ([`is_malformed_generation`]). The single-shot lanes have no agent loop to
/// carry [`MalformedGenerationHook`]; every other outcome comes back untouched on the
/// first attempt.
pub async fn retry_malformed<T, F, Fut>(
    model: &str,
    mut call: F,
) -> Result<T, rig_core::ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, rig_core::ProviderError>>,
{
    let mut attempts = 0usize;
    loop {
        match call().await {
            Err(error)
                if attempts < MALFORMED_RETRIES
                    && is_malformed_generation(&error.to_string()) =>
            {
                attempts += 1;
                tracing::warn!(
                    model,
                    attempt = attempts,
                    retries = MALFORMED_RETRIES,
                    %error,
                    "the provider could not parse this turn's generation — sending the same \
                     request again"
                );
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
    use std::time::Duration;

    fn post() -> Request<Bytes> {
        Request::post("https://example.test/v1/chat")
            .header("x-test", "1")
            .body(Bytes::from_static(b"{\"model\":\"m\"}"))
            .expect("a valid request")
    }

    /// A client that answers with `statuses` in order, then a 200.
    fn flaky(statuses: &[u16]) -> SequencedHttpClient {
        let mut responses: Vec<MockHttpResponse> = statuses
            .iter()
            .map(|s| {
                MockHttpResponse::error(
                    http::StatusCode::from_u16(*s).expect("valid status"),
                    "Please try again in 2s.",
                )
            })
            .collect();
        responses.push(MockHttpResponse::success(&b"ANSWER"[..]));
        SequencedHttpClient::new(responses)
    }

    /// The whole point of the transient class: one 429 costs a second request, not the
    /// turn — and the wait before it honors the provider's own delay hint (2s, longer
    /// than the first attempt's 1s backoff ceiling, so the wait is deterministic
    /// regardless of jitter).
    #[tokio::test(start_paused = true)]
    async fn a_transient_status_waits_then_the_next_attempt_answers() {
        let inner = flaky(&[429]);
        let http = RetryingHttp::new(inner.clone(), "m");
        let start = tokio::time::Instant::now();
        let response = http
            .send::<Bytes, Bytes>(post())
            .await
            .expect("the retry answers after waiting");
        let body = response.into_body().await.expect("body");
        assert_eq!(&body[..], b"ANSWER");
        assert_eq!(
            inner.requests().len(),
            2,
            "one 429 costs exactly one extra request"
        );
        assert!(
            start.elapsed() >= TRANSIENT_BACKOFF_FLOOR,
            "the provider's delay hint (2s) must be honored: {:?}",
            start.elapsed()
        );
    }

    /// A retry sends the same request the failed attempt sent — the transcript is what
    /// makes this recovery worth having, so losing it would defeat the module.
    #[tokio::test(start_paused = true)]
    async fn a_retry_sends_the_identical_request() {
        let inner = flaky(&[503, 529]);
        let http = RetryingHttp::new(inner.clone(), "m");
        http.send::<Bytes, Bytes>(post()).await.expect("answers");
        let sent = inner.requests();
        assert_eq!(sent.len(), 3);
        for pair in sent.windows(2) {
            assert_eq!(pair[0].uri, pair[1].uri);
            assert_eq!(pair[0].body, pair[1].body, "the body is resent byte for byte");
            assert_eq!(pair[0].headers, pair[1].headers, "the headers are resent too");
        }
    }

    /// Bounded. A provider that reports a transient failure on every attempt fails the
    /// request after [`TRANSIENT_RETRIES`] further requests, carrying its own words up.
    #[tokio::test(start_paused = true)]
    async fn a_provider_that_always_reports_transient_failure_fails_after_the_bound() {
        let inner = flaky(&[503; TRANSIENT_RETRIES + 2]);
        let http = RetryingHttp::new(inner.clone(), "m");
        let error = http
            .send::<Bytes, Bytes>(post())
            .await
            .err()
            .expect("a provider that is always overloaded must still fail");
        assert_eq!(
            inner.requests().len(),
            TRANSIENT_RETRIES + 1,
            "the first request plus TRANSIENT_RETRIES, and no more"
        );
        assert_eq!(error.non_success_status().map(|s| s.as_u16()), Some(503));
        assert!(error.to_string().contains("try again in 2s"), "{error}");
    }

    /// The transient class must not widen what a rejected request costs: a 400 is not in
    /// [`TRANSIENT_TRANSPORT_STATUSES`], so it is one attempt, error up.
    #[tokio::test]
    async fn a_rejected_status_is_never_sent_again() {
        let inner = flaky(&[400, 401]);
        let http = RetryingHttp::new(inner.clone(), "m");
        let error = http
            .send::<Bytes, Bytes>(post())
            .await
            .err()
            .expect("rejected");
        assert_eq!(inner.requests().len(), 1, "a rejection costs one request");
        assert_eq!(error.non_success_status().map(|s| s.as_u16()), Some(400));
    }

    /// A `Retry-After` header steers the wait: the old model-level wrapper could never
    /// see one.
    #[tokio::test(start_paused = true)]
    async fn the_retry_after_header_sets_the_wait() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "7".parse().expect("header value"));
        let inner = SequencedHttpClient::new([
            MockHttpResponse::error_with_headers(
                http::StatusCode::TOO_MANY_REQUESTS,
                "slow down",
                headers,
            ),
            MockHttpResponse::success(&b"ANSWER"[..]),
        ]);
        let http = RetryingHttp::new(inner, "m");
        let start = tokio::time::Instant::now();
        http.send::<Bytes, Bytes>(post()).await.expect("answers");
        assert!(
            start.elapsed() >= Duration::from_secs(7),
            "the header's 7s is honored, not the jittered 1s ceiling: {:?}",
            start.elapsed()
        );
    }

    /// The transient vocabulary: the statuses that must fire and the near neighbors
    /// that must not, including the 2xx trap named in the module doc.
    #[test]
    fn the_transient_vocabulary_covers_the_shipped_statuses_and_no_more() {
        for status in [429, 500, 502, 503, 529] {
            assert!(is_transient_status(status), "{status} is transient");
        }
        for status in [200, 400, 401, 403, 404, 422] {
            assert!(!is_transient_status(status), "{status} is not transient");
        }
    }

    /// The malformed vocabulary, across the spellings Gemini's wire produces — and the
    /// neighbors that must stay out of it.
    #[test]
    fn the_malformed_vocabulary_covers_the_shipped_spellings_and_no_more() {
        for text in [
            "ResponseError: Gemini stopped with finish_reason=MalformedFunctionCall: \
             malformed function call: default_api",
            "MALFORMED_FUNCTION_CALL",
            "ProviderError: {\"finishReason\":\"MALFORMED_RESPONSE\"}",
        ] {
            assert!(is_malformed_generation(text), "must be recognized: {text}");
        }
        for text in [
            "ProviderError: {\"type\":\"overloaded_error\"}",
            "ProviderError: invalid_request_error",
            "ResponseError: Response did not contain a valid message or tool call",
            "ResponseError: Gemini stopped with finish_reason=MissingThoughtSignature: none",
            "HttpError: error sending request: operation timed out",
        ] {
            assert!(!is_malformed_generation(text), "must stay out: {text}");
        }
    }

    /// The turn rule: a malformed finish reason on a tool-free turn is retried; a turn
    /// holding a tool call, or any other finish reason, is accepted.
    #[test]
    fn only_a_tool_free_malformed_turn_is_retried() {
        let malformed = FinishReason::Other("MALFORMED_FUNCTION_CALL".into());
        let text = vec![AssistantContent::text("hi")];
        let call = vec![AssistantContent::tool_call(
            "c1",
            rig_core::message::ToolName::new("run_kaish").expect("name"),
            serde_json::json!({}),
        )];
        assert!(malformed_turn(Some(&malformed), &text));
        assert!(
            !malformed_turn(Some(&malformed), &call),
            "a tool call is never retried"
        );
        assert!(!malformed_turn(Some(&FinishReason::Stop), &text));
        assert!(!malformed_turn(
            Some(&FinishReason::Other("SAFETY".into())),
            &text
        ));
        assert!(!malformed_turn(None, &text));
    }

    /// `retry_malformed` is bounded and passes every other failure through at once.
    #[tokio::test]
    async fn retry_malformed_is_bounded_and_ignores_other_errors() {
        let tries = AtomicUsize::new(0);
        let error = retry_malformed("m", || {
            tries.fetch_add(1, Ordering::SeqCst);
            async {
                Err::<(), _>(rig_core::ProviderError::Response(
                    "Gemini stopped with finish_reason=MalformedFunctionCall: x".into(),
                ))
            }
        })
        .await
        .expect_err("a provider that always fumbles still fails");
        assert_eq!(tries.load(Ordering::SeqCst), MALFORMED_RETRIES + 1);
        assert!(error.to_string().contains("MalformedFunctionCall"), "{error}");

        let tries = AtomicUsize::new(0);
        let _ = retry_malformed("m", || {
            tries.fetch_add(1, Ordering::SeqCst);
            async {
                Err::<(), _>(rig_core::ProviderError::Provider(
                    "invalid_request_error".into(),
                ))
            }
        })
        .await;
        assert_eq!(tries.load(Ordering::SeqCst), 1, "a rejection costs one request");

        let tries = AtomicUsize::new(0);
        let ok = retry_malformed("m", || {
            let n = tries.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(rig_core::ProviderError::Response(
                        "MALFORMED_FUNCTION_CALL".into(),
                    ))
                } else {
                    Ok("ANSWER")
                }
            }
        })
        .await
        .expect("the second attempt answers");
        assert_eq!(ok, "ANSWER");
    }

    /// The two OpenAI 429 spellings kaibo has seen, parsed to the seconds they name.
    #[test]
    fn delay_hint_seconds_parses_the_shipped_openai_spellings() {
        assert_eq!(delay_hint_seconds("Please try again in 20s."), Some(20.0));
        assert_eq!(
            delay_hint_seconds("Rate limit reached for gpt-4o. Please try again in 1.2s."),
            Some(1.2)
        );
    }

    /// Every other body shape falls back to `None` cleanly, never a panic and never a
    /// fabricated number.
    #[test]
    fn delay_hint_seconds_falls_back_cleanly_on_anything_that_does_not_parse() {
        for body in [
            "",
            "Overloaded",
            "{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}",
            "try again in twenty seconds",
            "try again in 20 seconds",
            "try again in s",
        ] {
            assert_eq!(delay_hint_seconds(body), None, "{body:?}");
        }
    }

    /// A hint too large for a `Duration` is capped, never a panic. Twenty digits of
    /// seconds is finite as an `f64` yet larger than `Duration` holds — the cap must
    /// apply to the number before it becomes a `Duration`.
    #[test]
    fn an_absurd_delay_hint_is_capped_not_a_panic() {
        let wait = transient_wait(1, None, Some("Please try again in 99999999999999999999s."));
        assert!(wait <= TRANSIENT_BACKOFF_CAP, "{wait:?}");
    }
}
