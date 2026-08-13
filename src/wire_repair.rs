//! Repairing one provider response shape rig-core cannot decode.
//!
//! vLLM serves a chat-completions assistant message carrying **both** spellings of
//! the reasoning field:
//!
//! ```json
//! {"role":"assistant","content":"391","reasoning":null,"reasoning_content":null}
//! ```
//!
//! rig-core 0.41 declares one field for the two
//! (`#[serde(rename = "reasoning_content", alias = "reasoning")]`,
//! `providers/openai/completion/mod.rs`), so both keys land on the same field and
//! serde raises `duplicate field \`reasoning_content\``. The decode failure does not
//! surface as a decode failure: rig's `ApiResponse` is untagged, so a body that fails
//! the success shape falls through to the error arm, and `from_http_response`
//! preserves a 2xx as `CompletionError::ProviderResponse`. A complete answer with
//! `finish_reason: "stop"` reaches the caller as a hard provider error.
//!
//! Measured 2026-08-13 against Crusoe (`Qwen/Qwen3-235B-A22B-Instruct-2507`), and the
//! same call fails identically on kaibo 0.2.0 — this is latent, not a regression. It
//! is also not a Crusoe quirk: the emitter is vLLM, so every vLLM-backed
//! OpenAI-compatible endpoint carries it (Together, Fireworks, self-hosted).
//!
//! **Why this repairs the response instead of the request.** Nothing kaibo can put on
//! the request stops a server from sending both keys — the shape is the server's, not
//! a reply to an option we set. The decode happens inside rig, so the last place kaibo
//! owns the bytes is the HTTP client it already injects (`Arm::from_slot` passes its
//! own client to every provider builder). [`Repaired`] is that seam: a transparent
//! [`HttpClientExt`] that hands rig a body rig can parse.
//!
//! **The repair is narrow on purpose.** [`repair_duplicate_reasoning`] changes a body
//! only when one message object holds both keys — the exact shape that fails — and
//! returns `None` for everything else, so an unrecognized body reaches rig byte-for-byte.
//! It never invents a field, never drops a field that carries text unless the other
//! spelling carries the same text, and logs at `warn` on the one path where two
//! different non-null values force a choice. A body kaibo does not understand is a body
//! kaibo does not touch.
//!
//! **Revisit at the next rig bump.** Amy, 2026-08-13: *"we will work around and revisit
//! next time we bump rig."* If rig learns to accept both spellings, this module and its
//! call sites in [`crate::consult::engine`] go away — [`repairs_nothing_when_rig_can_already_decode`]
//! is the test that will say so, because it decodes the captured payload with rig's own
//! type and fails the day rig stops needing help.

use bytes::Bytes;
use rig_core::http_client::{self, HttpClientExt, LazyBody, Request, Response};
use serde_json::Value;

/// The canonical spelling — rig's `rename`, and what the repair keeps.
const CANONICAL: &str = "reasoning_content";
/// The alias rig also accepts, and the key the repair removes when both are present.
const ALIAS: &str = "reasoning";

/// Rewrite a chat-completions body that carries both reasoning spellings on one
/// message, or `None` when there is nothing to repair.
///
/// `None` is the overwhelmingly common answer and means *hand rig the original bytes*.
/// It covers every body that is not JSON, is not an object, has no `choices`, or holds
/// only one of the two keys — including the Responses wire, whose payload has no
/// `choices` at all.
pub fn repair_duplicate_reasoning(body: &[u8]) -> Option<Bytes> {
    // Both keys must appear literally before it is worth parsing. Every response
    // pays this scan and almost none pay the parse; `reasoning_content` contains
    // `reasoning`, so the cheap test is one `find` for each spelling's quoted key.
    let looks_relevant =
        memchr_contains(body, b"\"reasoning_content\"") && memchr_contains(body, b"\"reasoning\"");
    if !looks_relevant {
        return None;
    }

    let mut doc: Value = serde_json::from_slice(body).ok()?;
    let choices = doc.get_mut("choices")?.as_array_mut()?;

    let mut repaired = false;
    for choice in choices.iter_mut() {
        let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut) else {
            continue;
        };
        if !(message.contains_key(CANONICAL) && message.contains_key(ALIAS)) {
            continue;
        }
        let alias = message.get(ALIAS).cloned().unwrap_or(Value::Null);
        let canonical = message.get(CANONICAL).cloned().unwrap_or(Value::Null);
        // Keep whichever spelling actually carries the reasoning. When only the alias
        // does, its text is promoted onto the canonical key rather than discarded —
        // dropping it would lose a block rig would otherwise have surfaced.
        if canonical.is_null() && !alias.is_null() {
            message.insert(CANONICAL.to_string(), alias.clone());
        } else if !canonical.is_null() && !alias.is_null() && canonical != alias {
            // Two different reasoning texts on one message. Undefined by every
            // provider doc we have; the canonical spelling wins because that is the
            // field rig names, and the choice is logged because it discards text.
            tracing::warn!(
                "provider sent different `reasoning` and `reasoning_content` values on one \
                 message; keeping `reasoning_content`"
            );
        }
        message.remove(ALIAS);
        repaired = true;
    }

    if !repaired {
        return None;
    }
    tracing::debug!("repaired a response carrying both reasoning spellings");
    serde_json::to_vec(&doc).ok().map(Bytes::from)
}

/// Substring search over raw bytes, without pulling a dependency for it.
fn memchr_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// An [`HttpClientExt`] that repairs a response body rig-core cannot decode, and is
/// otherwise the inner client exactly.
///
/// Transparent on every other path: the request, the status, the headers, and each
/// error are the inner client's own, and a body with nothing to repair is forwarded
/// byte-for-byte.
///
/// `Default` is required by rig: its client builders bound the HTTP backend on it, so
/// a wrapper without it cannot be injected at all.
#[derive(Clone, Debug, Default)]
pub struct Repaired<H> {
    inner: H,
}

impl<H> Repaired<H> {
    /// Wrap `inner` so a chat-completions body carrying both reasoning spellings is
    /// repaired before rig parses it.
    pub fn new(inner: H) -> Self {
        Self { inner }
    }
}

impl<H: HttpClientExt + Clone> HttpClientExt for Repaired<H> {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        // Ask the inner client for `Bytes` rather than `U`: the repair needs the raw
        // body, and `U: From<Bytes>` lets us build the caller's type afterwards either
        // way. This is why the wrapper can sit under a generic provider client.
        let inner = self.inner.send::<T, Bytes>(req);
        async move {
            let response = inner.await?;
            let (parts, body) = response.into_parts();
            let repaired: LazyBody<U> = Box::pin(async move {
                let bytes = body.await?;
                let bytes = repair_duplicate_reasoning(&bytes).unwrap_or(bytes);
                Ok(U::from(bytes))
            });
            Ok(Response::from_parts(parts, repaired))
        }
    }

    // Neither path below carries a chat-completions body: kaibo never streams (rig's
    // prompt loop is non-streaming) and never posts a multipart form on a completion.
    // They forward untouched so the wrapper stays a drop-in for the whole trait.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::providers::openai::completion::CompletionResponse;

    /// The response captured live from Crusoe on 2026-08-13, verbatim apart from
    /// shortening the answer text. Both reasoning spellings, both null,
    /// `finish_reason: "stop"` — a complete answer rig-core 0.41 refuses to decode.
    const CAPTURED_VLLM: &str = r#"{"id":"chatcmpl-fde1dc7d99a24bb1911e3c1355595ea1","object":"chat.completion","created":1786650747,"model":"Qwen/Qwen3-235B-A22B-Instruct-2507","choices":[{"index":0,"message":{"role":"assistant","content":"391","refusal":null,"annotations":null,"audio":null,"function_call":null,"tool_calls":[],"reasoning":null,"reasoning_content":null},"logprobs":null,"finish_reason":"stop","stop_reason":null,"token_ids":null}],"service_tier":null,"system_fingerprint":null,"usage":{"prompt_tokens":163,"total_tokens":221,"completion_tokens":58,"prompt_tokens_details":null},"prompt_logprobs":null,"prompt_token_ids":null,"kv_transfer_params":null}"#;

    fn decodes_with_rig(body: &[u8]) -> Result<CompletionResponse, serde_json::Error> {
        serde_json::from_slice::<CompletionResponse>(body)
    }

    /// The bug, stated as a test: rig cannot decode what vLLM sent. This is the
    /// failing-first assertion the repair exists to satisfy — and the guard that says
    /// when the workaround can be deleted. If a rig bump makes this decode on its own,
    /// this test fails and [`Repaired`] should go.
    #[test]
    fn repairs_nothing_when_rig_can_already_decode() {
        let err = decodes_with_rig(CAPTURED_VLLM.as_bytes()).expect_err(
            "rig-core 0.41 cannot decode both reasoning spellings — if this now \
                         decodes, rig fixed it upstream and `wire_repair` can be deleted",
        );
        assert!(
            err.to_string().contains("duplicate field"),
            "expected the duplicate-field decode failure, got: {err}"
        );
    }

    #[test]
    fn the_captured_vllm_response_decodes_after_repair() {
        let repaired = repair_duplicate_reasoning(CAPTURED_VLLM.as_bytes())
            .expect("the captured body carries both spellings, so it must be repaired");
        let decoded = decodes_with_rig(&repaired).expect("rig decodes the repaired body");
        // The answer survived the repair — the whole point is that this text was
        // never lost, only unreachable. rig re-serializes `content` as typed blocks,
        // so the text is read out of the first one rather than off a bare string.
        let json = serde_json::to_value(&decoded).unwrap();
        assert_eq!(json["choices"][0]["message"]["content"][0]["text"], "391");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn a_body_with_one_spelling_is_left_alone() {
        for body in [
            r#"{"choices":[{"message":{"role":"assistant","content":"x","reasoning_content":"why"}}]}"#,
            r#"{"choices":[{"message":{"role":"assistant","content":"x","reasoning":"why"}}]}"#,
            r#"{"choices":[{"message":{"role":"assistant","content":"x"}}]}"#,
        ] {
            assert!(
                repair_duplicate_reasoning(body.as_bytes()).is_none(),
                "nothing to repair in {body}"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_a_chat_completion_is_left_alone() {
        for body in [
            // Not JSON at all.
            "<html>502 Bad Gateway</html>",
            // JSON, but no `choices` — the Responses wire, and every error envelope.
            r#"{"error":{"message":"reasoning and reasoning_content are unsupported"}}"#,
            r#"{"output":[{"content":[{"text":"hi"}]}]}"#,
            // `choices` present but not an array of message objects.
            r#"{"choices":"reasoning_content and reasoning"}"#,
        ] {
            assert!(
                repair_duplicate_reasoning(body.as_bytes()).is_none(),
                "nothing to repair in {body}"
            );
        }
    }

    /// The alias carrying the only reasoning text is promoted, not discarded — rig
    /// would have surfaced that block, so the repair must not cost the caller a
    /// reasoning trace.
    #[test]
    fn reasoning_text_on_the_alias_survives_under_the_canonical_key() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"x","reasoning":"because","reasoning_content":null}}]}"#;
        let repaired = repair_duplicate_reasoning(body.as_bytes()).expect("both keys present");
        let doc: Value = serde_json::from_slice(&repaired).unwrap();
        let message = &doc["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "because");
        assert!(
            message.get("reasoning").is_none(),
            "the duplicate key must be gone, or rig still fails to decode"
        );
    }

    #[test]
    fn the_canonical_spelling_wins_when_both_carry_different_text() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"x","reasoning":"alias text","reasoning_content":"canonical text"}}]}"#;
        let repaired = repair_duplicate_reasoning(body.as_bytes()).expect("both keys present");
        let doc: Value = serde_json::from_slice(&repaired).unwrap();
        assert_eq!(
            doc["choices"][0]["message"]["reasoning_content"],
            "canonical text"
        );
    }

    /// Every choice is repaired, not just the first — rig decodes the whole array, so
    /// a duplicate key on choice 2 fails the decode exactly as one on choice 0 does.
    #[test]
    fn every_choice_is_repaired() {
        let body = r#"{"choices":[
            {"message":{"role":"assistant","content":"a","reasoning":null,"reasoning_content":null}},
            {"message":{"role":"assistant","content":"b","reasoning":null,"reasoning_content":null}}
        ]}"#;
        let repaired = repair_duplicate_reasoning(body.as_bytes()).expect("both keys present");
        let doc: Value = serde_json::from_slice(&repaired).unwrap();
        for i in 0..2 {
            assert!(
                doc["choices"][i]["message"].get("reasoning").is_none(),
                "choice {i} still carries the duplicate key"
            );
        }
    }
}
