//! One retry when the provider could not parse what the model generated.
//!
//! There is a failure class that looks like a hard error and is really a coin that
//! landed wrong: the model fumbled a single tool call, the provider refused to shape
//! the response, and the request that produced it is still perfectly good. Gemini
//! names it `finishReason: MALFORMED_FUNCTION_CALL`, and rig-core 0.41 turns that into
//! a `CompletionError::ResponseError("Gemini stopped with finish_reason=
//! MalformedFunctionCall: …")` while parsing the response
//! (`providers/gemini/completion.rs`, `function_call_finish_reason_error`). Observed
//! live: a Gemini Flash explorer died on one empty function call twenty turns into a
//! `deliberate`, and the whole investigation went with it.
//!
//! **Why this wraps the model instead of catching the error in the loop.** rig-agent
//! yields a completion failure straight out of its driver stream
//! (`agent/runner.rs::run_model_turn`), and the `PromptError::CompletionError` it
//! becomes carries **no `chat_history`** — unlike `MaxTurnsError` and
//! `PromptCancelled`, which hand the transcript back and are exactly why kaibo can
//! recover from a turn cap or a `view_image` break. So by the time this failure
//! reaches [`crate::consult::run_phase`], the turns are already gone and a retry
//! there could only mean re-running the phase from the first prompt: paying for the
//! whole investigation again to reach the state we just lost. `CompletionModel` is a
//! trait, so wrapping the model puts the retry *underneath* the loop, where the
//! request still holds the entire transcript and rig never learns anything went
//! wrong. [`crate::completion_watch::Watched`] is the same seam used for observation;
//! this is its acting sibling, and it is the only thing in kaibo that sends a provider
//! request twice.
//!
//! **What is retried, and why that is not backoff.** This retries *the model's turn*:
//! [`MALFORMED_RETRIES`] further attempts, each sending the identical request with no
//! wait between them, and then the error goes up as before. Sending at once is the
//! point — the provider is not busy, one generation came out wrong, and the next
//! sampling of the same request is a fresh chance at it. Waiting is what a *transport*
//! failure wants, and kaibo still does not retry a rate-limited or overloaded call at
//! all; that is tracked as an upstream rig contribution in `docs/issues.md`. The retry
//! also spends none of rig's turn budget, since rig never sees the failed attempt.
//!
//! **Detection is a heuristic on the error text**, by the same necessity as
//! `server/render.rs::classify_failure`: the reason arrives inside a formatted
//! message rather than as a typed variant, so [`is_malformed_generation`] matches the
//! spellings providers ship. Two neighbors were considered and deliberately left out.
//! The OpenAI-family "Response did not contain a valid message or tool call"
//! (`providers/openai/completion/mod.rs`, `deepseek.rs`, `openrouter/completion.rs`)
//! also means "unparseable generation", but nothing yet says whether it is a fumble
//! or a refusal, and a wrong guess spends three calls on every refusal. Gemini's
//! `MissingThoughtSignature` is worse: if it fires because a replayed history dropped
//! a signature, it fires on every attempt, so retrying it burns the budget on every
//! call rather than rescuing anything. Add either one on evidence from a live probe.

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

/// A completion model that asks again, up to a bound, when the provider could not
/// parse the generation — and forwards every other outcome untouched.
///
/// Transparent on every path that is not a malformed generation: the response, the
/// usage, the `raw_response`, and each other error are the inner model's own.
#[derive(Clone, Debug)]
pub struct Retried<M> {
    inner: M,
    /// The model id, for the warn event — a count of these per model is the whole
    /// point of making the retry observable.
    model: String,
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
/// [`MALFORMED_RETRIES`] times — the drop-in at a phase's model call site, mirroring
/// [`watched`](crate::completion_watch::watched).
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
        for attempt in 1..=self.retries {
            match self.inner.completion(request.clone()).await {
                Err(error) if is_malformed_generation(&error.to_string()) => {
                    tracing::warn!(
                        model = %self.model,
                        attempt,
                        retries = self.retries,
                        %error,
                        "the provider could not parse this turn's tool call — sending \
                         the same request again"
                    );
                }
                outcome => return outcome,
            }
        }
        // The last attempt, un-cloned: a further malformed generation is the phase's
        // failure now, and it goes up carrying the provider's own words.
        self.inner.completion(request).await
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
    use crate::test_support::{provider_error, response_error, text_response, ScriptedClient};
    use rig_core::client::CompletionClient;
    use rig_core::completion::message::Message;
    use rig_core::OneOrMany;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
}
