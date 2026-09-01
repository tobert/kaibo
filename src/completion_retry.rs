//! Two retries when the request was fine and the provider was not: one when it could
//! not parse what the model generated, one when it was rate-limited or overloaded.
//!
//! **Malformed generation.** A failure class that looks like a hard error and is
//! really a coin that landed wrong: the model fumbled a single tool call, the
//! provider refused to shape the response, and the request that produced it is still
//! perfectly good. Gemini names it `finishReason: MALFORMED_FUNCTION_CALL`, and
//! rig-core 0.41 turns that into a `CompletionError::ResponseError("Gemini stopped
//! with finish_reason=MalformedFunctionCall: …")` while parsing the response
//! (`providers/gemini/completion.rs`, `function_call_finish_reason_error`). Observed
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
//! **Why this wraps the model instead of catching the error in the loop.** rig-agent
//! yields a completion failure straight out of its driver stream
//! (`agent/runner.rs::run_model_turn`), and the `PromptError::CompletionError` it
//! becomes carries **no `chat_history`** — unlike `MaxTurnsError` and
//! `PromptCancelled`, which hand the transcript back and are exactly why kaibo can
//! recover from a turn cap or a `view_image` break. So by the time either failure
//! reaches [`crate::consult::run_phase`], the turns are already gone and a retry
//! there could only mean re-running the phase from the first prompt: paying for the
//! whole investigation again to reach the state we just lost. `CompletionModel` is a
//! trait, so wrapping the model puts the retry *underneath* the loop, where the
//! request still holds the entire transcript and rig never learns anything went
//! wrong. [`crate::completion_watch::Watched`] is the same seam used for observation;
//! this is its acting sibling, and it is the only thing in kaibo that sends a provider
//! request twice.
//!
//! **What is retried, and why one class waits and the other does not.** This retries
//! *the model's turn*, not the whole phase — [`MALFORMED_RETRIES`] further attempts
//! for a malformed generation, [`TRANSIENT_RETRIES`] for a transient transport
//! failure, each counted independently so a turn that hits one of each spends both
//! allowances rather than sharing one. A malformed generation is sent again at once,
//! with no wait between attempts: the provider is not busy, one generation came out
//! wrong, and the next sampling of the same request is a fresh chance at it. A
//! transient transport failure waits first, because sending it again immediately
//! would just land in the same queue that just refused it — see the classification
//! and backoff section below. Either retry spends none of rig's turn budget, since
//! rig never sees the failed attempt.
//!
//! **Malformed-generation detection is a heuristic on the error text**, by the same
//! necessity as `server/render.rs::classify_failure`: the reason arrives inside a
//! formatted message rather than as a typed variant, so [`is_malformed_generation`]
//! matches the spellings providers ship. Two neighbors were considered and
//! deliberately left out. The OpenAI-family "Response did not contain a valid message
//! or tool call" (`providers/openai/completion/mod.rs`, `deepseek.rs`,
//! `openrouter/completion.rs`) also means "unparseable generation", but nothing yet
//! says whether it is a fumble or a refusal, and a wrong guess spends three calls on
//! every refusal. Gemini's `MissingThoughtSignature` is worse: if it fires because a
//! replayed history dropped a signature, it fires on every attempt, so retrying it
//! burns the budget on every call rather than rescuing anything. Add either one on
//! evidence from a live probe.
//!
//! **Transient-transport classification reads the status, not the text.**
//! [`is_transient_transport_failure`] leads with
//! `CompletionError::provider_response_status()` and matches only the specific codes
//! in [`TRANSIENT_TRANSPORT_STATUSES`] — never a bare `!is_success()`, because rig
//! 0.41's own docs note this accessor can carry a **2xx** status when a provider
//! ships an error envelope alongside a success response, and treating any non-2xx (or
//! any status at all) as transient would retry requests that were rejected on
//! purpose. The [`TRANSIENT_TRANSPORT_MARKERS`] text fallback fires only when no
//! status survived the error path at all — a transport that reports an error with no
//! HTTP status behind it (a gRPC/SDK client, or a gateway that swallows the code).
//!
//! **Waiting is exponential backoff with full jitter**, plus a best-effort assist
//! from the provider's own words. [`TRANSIENT_BACKOFF_FLOOR`],
//! [`TRANSIENT_BACKOFF_FACTOR`], and [`TRANSIENT_BACKOFF_CAP`] set the schedule; full
//! jitter (a uniform random wait between zero and each attempt's ceiling, the AWS
//! formula) is deliberate over an evenly-spaced wait because kaibo's own bursts come
//! from rig's `buffer_unordered` tool-call fan-out — several arms can hit one
//! backend's rate limit at the same moment, and full jitter is the shape that best
//! de-synchronizes a herd that started together. When the body embeds its own delay
//! (OpenAI's 429s read "…Please try again in 20s."), [`delay_hint_seconds`] parses it
//! and the wait becomes whichever is longer, still capped — never shorter than the
//! computed backoff, so a hint that under-promises can't turn into a busier retry
//! than kaibo would have chosen on its own. The header form of this same signal,
//! `Retry-After`, never reaches here: rig's non-streaming error path
//! (`http_client/mod.rs::non_success_status_error`) keeps only the status and the body
//! text, so a provider that puts the delay in a header rather than the body (a proper
//! Anthropic 429) gets the computed backoff alone. One `tracing::warn!` per wait names
//! the model, the status when one survived, the attempt, and the delay — the count
//! per model and status is the observability payoff, the same as the
//! malformed-generation warn above it.

use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
};

/// Attempts a turn gets *after* the first, when the provider could not parse the
/// generation. Two, so a turn costs at most three requests: one glitch is the observed
/// shape, a second in a row is unlucky, and a third says the request itself is the
/// problem and the phase should fail with it.
pub const MALFORMED_RETRIES: usize = 2;

/// The spellings a provider uses when it could not parse the model's tool call or the
/// response around it, lowercased. Confirmed against the vendored rig-core 0.41
/// sources rather than guessed:
///
/// - `malformedfunctioncall` / `malformedresponse` — the `{reason:?}` Debug spelling
///   rig formats into `CompletionError::ResponseError`
///   (`providers/gemini/completion.rs::function_call_finish_reason_error`). This is
///   the one kaibo has seen in the wild.
/// - `malformed_function_call` / `malformed_response` — Gemini's own
///   `SCREAMING_SNAKE_CASE` wire values, which a gateway may forward verbatim.
/// - `malformed function call` — Gemini's `finishMessage` prose ("malformed function
///   call: default_api"), which a gateway may forward without the reason beside it.
const MALFORMED_MARKERS: &[&str] = &[
    "malformedfunctioncall",
    "malformed_function_call",
    "malformed function call",
    "malformedresponse",
    "malformed_response",
];

/// True when an error text says the provider could not parse what the model
/// generated. Shared with `server/render.rs::classify_failure` so one vocabulary
/// decides both the retry and the advice a caller finally reads — a marker added here
/// reaches both.
pub fn is_malformed_generation(error_text: &str) -> bool {
    let text = error_text.to_lowercase();
    MALFORMED_MARKERS.iter().any(|marker| text.contains(marker))
}

/// Further attempts a turn gets after the first, when the provider reports a
/// transient transport failure. Four: a burst can cost up to five requests to ride
/// out, and the backoff schedule below already spends up to a minute doing it — more
/// than that turns one busy provider into a stalled phase.
pub const TRANSIENT_RETRIES: usize = 4;

/// The first wait's ceiling, before jitter. One second: fast enough that a lone 429
/// costs a blink, and the base [`TRANSIENT_BACKOFF_FACTOR`] doubles from here.
pub const TRANSIENT_BACKOFF_FLOOR: std::time::Duration = std::time::Duration::from_secs(1);

/// The multiplier between one wait's ceiling and the next: 1s, 2s, 4s, 8s, doubling
/// until [`TRANSIENT_BACKOFF_CAP`] takes over.
pub const TRANSIENT_BACKOFF_FACTOR: u32 = 2;

/// The most any single wait runs, jitter and delay hint both included. Both the
/// exponential term and a parsed [`delay_hint_seconds`] are clamped here, so a
/// provider embedding "try again in 600s" is capped the same as an unbounded
/// exponential term would be.
pub const TRANSIENT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// HTTP statuses that mean "the provider was not ready for this request right now",
/// not "this request is wrong" — matched against
/// `CompletionError::provider_response_status()`, never inferred from `!is_success()`:
///
/// - `429 Too Many Requests` — every provider's rate-limit status.
/// - `500` / `502` / `503` — the generic "the backend is down or overloaded" family,
///   seen from OpenAI-compatible gateways.
/// - `529` — Anthropic's own "Overloaded" status, outside the standard registry.
const TRANSIENT_TRANSPORT_STATUSES: &[u16] = &[429, 500, 502, 503, 529];

/// The spellings a provider's error body uses for transient trouble when no HTTP
/// status survived the error path (`provider_response_status()` returned `None`),
/// lowercased. Each is a phrase, not a bare code, by the same posture
/// [`MALFORMED_MARKERS`] documents — a request id or a document that happens to
/// contain "429" must not fire this:
///
/// - `overloaded_error` — Anthropic's `{"type":"overloaded_error"}` error envelope,
///   the wire spelling behind status `529`.
/// - `rate limit` — the phrase OpenAI, Anthropic, and Gemini all put in a 429 body
///   ("Rate limit reached for …", "rate_limit_error").
/// - `too many requests` — the HTTP reason phrase for `429`, forwarded verbatim by
///   some OpenAI-compatible gateways that drop the numeric status.
const TRANSIENT_TRANSPORT_MARKERS: &[&str] =
    &["overloaded_error", "rate limit", "too many requests"];

/// True when a `CompletionError` is a transient transport failure — the provider is
/// rate-limiting or overloaded right now, not refusing the request. Leads with
/// [`TRANSIENT_TRANSPORT_STATUSES`] against `provider_response_status()`; falls back
/// to [`TRANSIENT_TRANSPORT_MARKERS`] on the error text only when that status is
/// absent. A rejected request (400/401/403/404/422) and everything else this doesn't
/// name keep the wrapper's default behavior: one attempt, error up.
pub fn is_transient_transport_failure(error: &CompletionError) -> bool {
    if let Some(status) = error.provider_response_status() {
        return TRANSIENT_TRANSPORT_STATUSES.contains(&status.as_u16());
    }
    let text = error.to_string().to_lowercase();
    TRANSIENT_TRANSPORT_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
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

/// This attempt's backoff ceiling before jitter: `TRANSIENT_BACKOFF_FLOOR *
/// TRANSIENT_BACKOFF_FACTOR^(attempt - 1)`, capped at `TRANSIENT_BACKOFF_CAP`.
/// `attempt` is 1 for the first wait after the original request.
fn backoff_ceiling(attempt: u32) -> std::time::Duration {
    let scaled = TRANSIENT_BACKOFF_FLOOR.as_secs_f64()
        * (TRANSIENT_BACKOFF_FACTOR as f64).powi(attempt as i32 - 1);
    std::time::Duration::from_secs_f64(scaled).min(TRANSIENT_BACKOFF_CAP)
}

/// The wait before the `attempt`th retry of a transient transport failure: exponential
/// backoff with full jitter — a uniform random duration between zero and this
/// attempt's [`backoff_ceiling`] — raised to the provider's own delay hint when
/// [`delay_hint_seconds`] finds one longer than that, and capped at
/// [`TRANSIENT_BACKOFF_CAP`] either way.
fn transient_wait(attempt: u32, error: &CompletionError) -> std::time::Duration {
    let ceiling = backoff_ceiling(attempt);
    let backoff =
        std::time::Duration::from_secs_f64(rand::random_range(0.0..=ceiling.as_secs_f64()));
    let hint = error
        .provider_response_body()
        .and_then(delay_hint_seconds)
        .filter(|seconds| *seconds >= 0.0);
    match hint {
        // Cap the number, then convert: a hint can be finite yet larger than a
        // `Duration` holds (twenty digits of seconds), and `from_secs_f64` panics on
        // overflow — so a `.min` on the converted value would run one step too late.
        Some(hint) => {
            let seconds = hint
                .max(backoff.as_secs_f64())
                .min(TRANSIENT_BACKOFF_CAP.as_secs_f64());
            std::time::Duration::from_secs_f64(seconds)
        }
        None => backoff,
    }
}

/// A completion model that asks again, up to a bound, when the provider could not
/// parse the generation or reported a transient transport failure — and forwards
/// every other outcome untouched.
///
/// Transparent on every path that is neither: the response, the usage, the
/// `raw_response`, and each other error are the inner model's own.
#[derive(Clone, Debug)]
pub struct Retried<M> {
    inner: M,
    /// The model id, for the warn event — a count of these per model is the whole
    /// point of making the retry observable.
    model: String,
    /// Further attempts for a malformed generation. The transient-transport bound is
    /// [`TRANSIENT_RETRIES`], a fixed constant rather than a field — one process-wide
    /// backoff schedule until evidence says a deployment needs its own.
    retries: usize,
}

impl<M> Retried<M> {
    /// Wrap `model` so a malformed generation gets `retries` further attempts.
    pub fn new(model: M, name: impl Into<String>, retries: usize) -> Self {
        Self {
            inner: model,
            model: name.into(),
            retries,
        }
    }
}

/// Wrap a completion model so a malformed generation is sent again
/// [`MALFORMED_RETRIES`] times and a transient transport failure waits and is sent
/// again [`TRANSIENT_RETRIES`] times — the drop-in at a phase's model call site,
/// mirroring [`watched`](crate::completion_watch::watched).
pub fn retried<M: CompletionModel>(model: M, name: &str) -> Retried<M> {
    Retried::new(model, name, MALFORMED_RETRIES)
}

impl<M: CompletionModel> CompletionModel for Retried<M> {
    type Response = M::Response;
    type StreamingResponse = M::StreamingResponse;
    type Client = M::Client;

    /// Unreachable, and loudly so — the same contract
    /// [`Watched::make`](crate::completion_watch::Watched) holds. `make` is only ever
    /// called through `CompletionClient::completion_model`, and no client's
    /// `CompletionModel` is a `Retried`; kaibo always wraps an already-built model
    /// with [`retried`]. Building one here would have to invent a model name and a
    /// bound nobody chose.
    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unreachable!("Retried is built by `retried(model, name)`, never by CompletionModel::make")
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Two independent counters, because a turn that fumbles once and then hits a
        // rate limit once must spend both allowances, not share one between classes.
        let mut malformed_attempts = 0usize;
        let mut transient_attempts = 0u32;
        loop {
            let error = match self.inner.completion(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            if malformed_attempts < self.retries && is_malformed_generation(&error.to_string()) {
                malformed_attempts += 1;
                tracing::warn!(
                    model = %self.model,
                    attempt = malformed_attempts,
                    retries = self.retries,
                    %error,
                    "the provider could not parse this turn's tool call — sending \
                     the same request again"
                );
                continue;
            }
            if (transient_attempts as usize) < TRANSIENT_RETRIES
                && is_transient_transport_failure(&error)
            {
                transient_attempts += 1;
                let wait = transient_wait(transient_attempts, &error);
                tracing::warn!(
                    model = %self.model,
                    status = error.provider_response_status().map(|s| s.as_u16()),
                    attempt = transient_attempts,
                    retries = TRANSIENT_RETRIES,
                    delay_ms = wait.as_millis() as u64,
                    %error,
                    "the provider reported a transient transport failure — waiting, \
                     then sending the same request again"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            // Neither class, or a class whose allowance is spent: the phase's
            // failure now, carrying the provider's own words.
            return Err(error);
        }
    }

    /// Forwarded verbatim. kaibo drives the non-streaming loop, and a streamed
    /// malformed call arrives as a stream event rather than a failed call — a
    /// different job than this one.
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<
        rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
        CompletionError,
    > {
        self.inner.stream(request).await
    }

    /// Forwarded: this is a provider capability, and the wrapper must not change what
    /// rig believes the model can do.
    fn composes_native_output_with_tools(&self) -> bool {
        self.inner.composes_native_output_with_tools()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        provider_error, response_error, status_error, text_response, ScriptedClient,
    };
    use rig_core::client::CompletionClient;
    use rig_core::completion::message::Message;
    use rig_core::OneOrMany;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A one-turn request, the shape rig's agent builds for a toolless call.
    fn req() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one(Message::user("q")),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        }
    }

    /// The exact text rig-core 0.41 produces for the failure this module exists for:
    /// `function_call_finish_reason_error` formats the Debug reason and the provider's
    /// finish message into a `ResponseError`.
    fn gemini_malformed() -> CompletionError {
        response_error(
            "Gemini stopped with finish_reason=MalformedFunctionCall: malformed \
             function call: default_api",
        )
    }

    /// A scripted model that fails its first `fail_first` attempts with `error`, then
    /// answers. The counter is the one thing a content-driven responder cannot
    /// express: a retry sends a **byte-identical** request, so attempt 1 and attempt 2
    /// differ in nothing but their order. Counting is therefore the behavior under
    /// test, not a shortcut around the harness's design.
    fn flaky_model(
        fail_first: usize,
        error: impl Fn() -> CompletionError + Send + Sync + 'static,
    ) -> (
        super::Retried<crate::test_support::ScriptedModel>,
        Arc<AtomicUsize>,
    ) {
        let sent = Arc::new(AtomicUsize::new(0));
        let seen = sent.clone();
        let client = ScriptedClient::builder()
            .on_model("m", move |_req| {
                if seen.fetch_add(1, Ordering::SeqCst) < fail_first {
                    Err(error())
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .build();
        (retried(client.completion_model("m"), "m"), sent)
    }

    /// The whole point: one malformed generation costs a second request, not the turn.
    #[tokio::test]
    async fn a_malformed_generation_is_sent_again_and_the_second_attempt_answers() {
        let (model, sent) = flaky_model(1, gemini_malformed);
        let response = model.completion(req()).await.expect("the retry answers");
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "one glitch costs exactly one extra request"
        );
        assert!(
            matches!(
                response.choice.first(),
                rig_core::completion::message::AssistantContent::Text(_)
            ),
            "the caller gets the second attempt's answer"
        );
    }

    /// Bounded. A provider that fumbles every attempt fails the turn after
    /// [`MALFORMED_RETRIES`] further requests, carrying its own words up.
    #[tokio::test]
    async fn retrying_stops_at_the_bound_and_the_provider_error_survives() {
        let (model, sent) = flaky_model(usize::MAX, gemini_malformed);
        let error = model
            .completion(req())
            .await
            .expect_err("a provider that always fumbles must still fail");
        assert_eq!(
            sent.load(Ordering::SeqCst),
            MALFORMED_RETRIES + 1,
            "the first request plus MALFORMED_RETRIES, and no more"
        );
        assert!(
            error.to_string().contains("MalformedFunctionCall"),
            "the provider's own words reach the caller: {error}"
        );
    }

    /// Every other failure passes straight through on the first attempt. A rejected
    /// request is not a coin that landed wrong, and sending it again would spend a
    /// caller's money to be refused twice more.
    #[tokio::test]
    async fn a_rejected_request_is_never_sent_again() {
        let (model, sent) = flaky_model(usize::MAX, || provider_error("invalid_request_error"));
        let error = model.completion(req()).await.expect_err("rejected");
        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "a rejection costs one request, not three"
        );
        assert!(error.to_string().contains("invalid_request_error"));
    }

    /// The vocabulary, across the spellings rig-core 0.41 and Gemini's wire actually
    /// produce — and the neighbors that must stay out of it.
    #[test]
    fn the_vocabulary_covers_the_shipped_spellings_and_no_more() {
        for text in [
            "ResponseError: Gemini stopped with finish_reason=MalformedFunctionCall: \
             malformed function call: default_api",
            "ResponseError: Gemini stopped with finish_reason=MalformedResponse: \
             malformed response from provider",
            "ProviderError: {\"finishReason\":\"MALFORMED_FUNCTION_CALL\"}",
            "ProviderError: {\"finishReason\":\"MALFORMED_RESPONSE\"}",
        ] {
            assert!(
                is_malformed_generation(text),
                "a malformed generation must be recognized: {text}"
            );
        }
        for text in [
            "ProviderError: {\"type\":\"overloaded_error\"}",
            "ProviderError: invalid_request_error",
            "ResponseError: Response did not contain a valid message or tool call",
            "ResponseError: Gemini stopped with finish_reason=MissingThoughtSignature: none",
            "HttpError: error sending request: operation timed out",
        ] {
            assert!(
                !is_malformed_generation(text),
                "only a malformed generation is sent again: {text}"
            );
        }
    }

    /// A retry sends the same request the failed attempt sent — the transcript is what
    /// makes this recovery worth having, so losing it would defeat the module.
    #[tokio::test]
    async fn a_retry_sends_the_identical_request() {
        let sent = Arc::new(AtomicUsize::new(0));
        let seen = sent.clone();
        let client = ScriptedClient::builder()
            .on_model("m", move |_req| {
                if seen.fetch_add(1, Ordering::SeqCst) < 1 {
                    Err(response_error(
                        "Gemini stopped with finish_reason=MalformedFunctionCall: x",
                    ))
                } else {
                    Ok(text_response("ANSWER"))
                }
            })
            .build();
        let model = retried(client.completion_model("m"), "m");
        let mut request = req();
        request.chat_history = OneOrMany::many([
            Message::user("the question"),
            Message::assistant("a partial investigation"),
        ])
        .expect("two messages");
        model.completion(request).await.expect("the retry answers");

        let asked = client.requests_for("m");
        assert_eq!(asked.len(), 2, "two requests were recorded");
        assert_eq!(
            serde_json::to_value(&asked[0].raw).unwrap_or(Value::Null),
            serde_json::to_value(&asked[1].raw).unwrap_or(Value::Null),
            "the retry is the same request, transcript and all"
        );
    }

    /// The whole point of the transient class: one 429 costs a second request, not
    /// the turn — and the wait before it honors the provider's own delay hint (2s,
    /// longer than the first attempt's 1s backoff ceiling, so the wait is
    /// deterministic regardless of jitter).
    #[tokio::test(start_paused = true)]
    async fn a_transient_transport_failure_waits_then_the_next_attempt_answers() {
        let (model, sent) = flaky_model(1, || status_error(429, "Please try again in 2s."));
        let start = tokio::time::Instant::now();
        let response = model
            .completion(req())
            .await
            .expect("the retry answers after waiting");
        let elapsed = start.elapsed();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "one 429 costs exactly one extra request"
        );
        assert!(
            matches!(
                response.choice.first(),
                rig_core::completion::message::AssistantContent::Text(_)
            ),
            "the caller gets the second attempt's answer"
        );
        assert!(
            elapsed >= TRANSIENT_BACKOFF_FLOOR,
            "the provider's delay hint (2s) must be honored, not the shorter jittered \
             backoff: {elapsed:?}"
        );
    }

    /// Bounded, like the malformed-generation class. A provider that reports a
    /// transient failure on every attempt fails the turn after [`TRANSIENT_RETRIES`]
    /// further requests, carrying its own words up. No delay hint here, so the wait
    /// is pure jittered backoff; the ceilings for attempts 1..=4 are 1s, 2s, 4s, 8s,
    /// so total elapsed can never exceed their sum even though jitter makes the exact
    /// value unpredictable.
    #[tokio::test(start_paused = true)]
    async fn a_provider_that_always_reports_transient_failure_fails_after_the_bound() {
        let (model, sent) = flaky_model(usize::MAX, || status_error(503, "backend overloaded"));
        let start = tokio::time::Instant::now();
        let error = model
            .completion(req())
            .await
            .expect_err("a provider that is always overloaded must still fail");
        let elapsed = start.elapsed();
        assert_eq!(
            sent.load(Ordering::SeqCst),
            TRANSIENT_RETRIES + 1,
            "the first request plus TRANSIENT_RETRIES, and no more"
        );
        assert!(
            error.to_string().contains("backend overloaded"),
            "the provider's own words reach the caller: {error}"
        );
        assert!(
            elapsed <= Duration::from_secs(1 + 2 + 4 + 8),
            "elapsed must stay within the sum of the four attempts' jitter ceilings: \
             {elapsed:?}"
        );
    }

    /// Regression: the new transient class must not widen what a rejected request
    /// costs. A 400 is not in [`TRANSIENT_TRANSPORT_STATUSES`], so it keeps the
    /// wrapper's default — one attempt, error up, exactly like the text-classified
    /// rejection above.
    #[tokio::test]
    async fn a_rejected_status_is_never_sent_again() {
        let (model, sent) = flaky_model(usize::MAX, || status_error(400, "invalid_request_error"));
        let error = model.completion(req()).await.expect_err("rejected");
        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "a rejected status costs one request, not five"
        );
        assert!(error.to_string().contains("invalid_request_error"));
    }

    /// The transient vocabulary: the statuses and text markers that must fire, the
    /// near neighbors that must not, and the two traps named in the module doc — a
    /// 2xx status carrying an error envelope, and a bare "429" substring with none of
    /// the marker phrases around it.
    #[test]
    fn the_transient_vocabulary_covers_the_shipped_statuses_and_markers_and_no_more() {
        for status in [429, 500, 502, 503, 529] {
            let error = status_error(status, "boom");
            assert!(
                is_transient_transport_failure(&error),
                "status {status} must be recognized as transient"
            );
        }
        for status in [400, 401, 403, 404, 422] {
            let error = status_error(status, "boom");
            assert!(
                !is_transient_transport_failure(&error),
                "status {status} is a rejection, not a transient failure"
            );
        }
        // A 2xx status carrying a provider error envelope must never be treated as
        // transient, even when its body contains a marker phrase — the status branch
        // is authoritative once it fires, never falling through to the text markers.
        let envelope = status_error(200, "{\"type\":\"overloaded_error\"}");
        assert!(
            !is_transient_transport_failure(&envelope),
            "a 2xx envelope status must not be inferred as failure from its body text"
        );
        for text_error in [
            provider_error("Anthropic: {\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}"),
            provider_error(
                "Rate limit reached for gpt-4 in organization org-x on requests per min",
            ),
            provider_error("429 Too Many Requests"),
        ] {
            assert!(
                is_transient_transport_failure(&text_error),
                "a marker fallback must fire when no status survived: {text_error}"
            );
        }
        for text_error in [
            provider_error("HttpError: error sending request: operation timed out"),
            response_error("Gemini stopped with finish_reason=MissingThoughtSignature: none"),
            status_error(404, "not found"),
            provider_error("request id 4293001 could not be traced"),
        ] {
            assert!(
                !is_transient_transport_failure(&text_error),
                "must not be recognized as transient: {text_error}"
            );
        }
    }

    /// The two OpenAI 429 spellings kaibo has seen, parsed to the seconds they name.
    #[test]
    fn delay_hint_seconds_parses_the_shipped_openai_spellings() {
        assert_eq!(
            delay_hint_seconds("Please try again in 20s."),
            Some(20.0),
            "the whole-second, trailing-period spelling"
        );
        assert_eq!(
            delay_hint_seconds("Rate limit reached for gpt-4o. Please try again in 1.2s."),
            Some(1.2),
            "the fractional-second spelling"
        );
    }

    /// Every other body shape — no hint at all, or a hint phrased in a way this
    /// pattern does not commit to — falls back to `None` cleanly, never a panic and
    /// never a fabricated number.
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
            assert_eq!(
                delay_hint_seconds(body),
                None,
                "must not parse a delay out of: {body:?}"
            );
        }
    }

    /// A hint too large for a `Duration` is capped, never a panic. Twenty digits of
    /// seconds is finite as an `f64` (so the finiteness guard passes) yet larger than
    /// `Duration`'s `u64::MAX`-seconds ceiling — the cap must apply to the number
    /// before it becomes a `Duration`, or the conversion itself dies first.
    #[test]
    fn an_absurd_delay_hint_is_capped_not_a_panic() {
        let error = status_error(429, "Please try again in 99999999999999999999s.");
        let wait = transient_wait(1, &error);
        assert!(
            wait <= TRANSIENT_BACKOFF_CAP,
            "a huge hint waits the cap, got {wait:?}"
        );
    }
}
