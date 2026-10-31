//! rig-core 0.46 decodes the vLLM chat-completions shape that 0.42 could not.
//!
//! vLLM serves an assistant message carrying **both** spellings of the reasoning field
//! (`"reasoning":null,"reasoning_content":null`). rig-core 0.42 declared one serde field
//! for the two (`rename` + `alias`), so both keys landed on it and decode died with
//! `duplicate field`; kaibo shipped `wire_repair` to rewrite the body before rig read it.
//! rig 0.46 decodes the wire with one typed decoder per wire, and this test is the proof
//! the workaround is no longer needed: the *unrepaired* payload, captured live from vLLM
//! (Crusoe, `Qwen/Qwen3-235B-A22B-Instruct-2507`, 2026-08-13), goes through a real
//! chat-completions model and comes back as a normal answer.
//!
//! If a future rig regresses this, this test fails first, and the fix is to bring a
//! body-repair wrapper back — not to weaken the assertion.

use rig_core::completion::CompletionRequest;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};

/// The captured response: both reasoning spellings, both null, `finish_reason: "stop"`.
const CAPTURED_VLLM: &str = r#"{"id":"chatcmpl-fde1dc7d99a24bb1911e3c1355595ea1","object":"chat.completion","created":1786650747,"model":"Qwen/Qwen3-235B-A22B-Instruct-2507","choices":[{"index":0,"message":{"role":"assistant","content":"391","refusal":null,"annotations":null,"audio":null,"function_call":null,"tool_calls":[],"reasoning":null,"reasoning_content":null},"logprobs":null,"finish_reason":"stop","stop_reason":null,"token_ids":null}],"service_tier":null,"system_fingerprint":null,"usage":{"prompt_tokens":163,"total_tokens":221,"completion_tokens":58,"prompt_tokens_details":null},"prompt_logprobs":null,"prompt_token_ids":null,"kv_transfer_params":null}"#;

#[tokio::test]
async fn both_reasoning_spellings_decode_without_a_repair_wrapper() {
    let http = SequencedHttpClient::new([MockHttpResponse::success(CAPTURED_VLLM.as_bytes().to_vec())]);
    let model = OpenAIConfig::new("test-key")
        .with_base_url("http://vllm.example/v1")
        .connect(http)
        .chat("Qwen/Qwen3-235B-A22B-Instruct-2507");

    let response = model
        .call(CompletionRequest::new("what is 17 * 23?"))
        .await
        .expect("rig decodes a body carrying both reasoning spellings");

    assert_eq!(response.text(), "391", "the answer text survived the decode");
    assert_eq!(response.usage.input_tokens, Some(163));
    assert_eq!(response.usage.output_tokens, Some(58));
}
