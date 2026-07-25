# OpenAI API integration plan

Living plan started 2026-07-25. Scope: make hosted OpenAI models a first-class kaibo
backend while preserving the generic OpenAI-compatible path for local servers and gateways.

## Entitlement answer

Use an OpenAI Platform API key for kaibo's OpenAI model calls. Do not tell users that a
Codex or ChatGPT subscription pays for kaibo's API traffic.

The split matters:

- Codex clients can sign in with ChatGPT subscription access. OpenAI's Codex help says
  Codex CLI / IDE / web / app use ChatGPT sign-in, and that users who previously used the
  CLI with an API key can switch to subscription-based access.
- OpenAI's billing help says ChatGPT and the API Platform use separate billing systems.
- The OpenAI API quickstart starts with creating/exporting an API key and adding API
  credits for real application use.

So the near-term product contract is:

```sh
export OPENAI_API_KEY="..."
```

plus a configured `kind = "openai"` backend pointed at `https://api.openai.com/v1`.

A future Codex-specific integration would be a separate surface. It would control Codex as
Codex, not act as a drop-in replacement for kaibo's provider-backed `CompletionModel` arms.

Sources:

- [Using Codex with your ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan.pdf)
- [Managing Billing Settings on ChatGPT Web and Platform](https://help.openai.com/en/articles/9039756-managing-your-work-in-the-api-platform-with-projects%25252525252525252525252525252525252525252525252525252525252525252525252525252525252525252525252525252525252525252525253F.docx)
- [OpenAI API quickstart](https://platform.openai.com/docs/quickstart/make-your-first-api-request)

## Current state in kaibo

- `ProviderKind::Openai` is the generic OpenAI-compatible wire path. It builds a
  `rig::providers::openai::CompletionsClient` with the backend's `base_url`
  (`src/consult/engine.rs`).
- The built-in `openai-local` backend defaults to a local server URL and optional key;
  hosted OpenAI already works by adding a new backend such as `gpt` with
  `base_url = "https://api.openai.com/v1"` and `key_optional = false`
  (`docs/config.md`, `docs/config.example.toml`).
- Vision is opt-in for generic `openai` slots. A hosted multimodal GPT slot must set
  `vision = true` until kaibo has a trustworthy OpenAI model-capability classifier.
- `view_image` already handles the OpenAI transport mismatch: OpenAI-compatible wires do
  not carry image bytes in tool results, so kaibo rewrites viewed images onto a user image
  turn before resuming the model loop (`src/consult/shaping.rs`,
  `src/consult/engine.rs`).
- Request shaping is incomplete for hosted OpenAI. `ProviderKind::Openai` currently maps
  to `ThinkingStyle::None`, so reasoning-capable GPT models do not receive OpenAI
  reasoning params. `docs/issues.md` already tracks this.
- OpenAI batch is not implemented. The existing batch lane supports Anthropic and Gemini;
  `docs/issues.md` tracks OpenAI's file-based batch shape as the next provider.

## Direction

Keep two paths distinct:

1. **Generic OpenAI-compatible** — local llama.cpp/Ollama/LM Studio gateways and other
   `/v1/chat/completions`-compatible servers. Preserve maximum compatibility. Do not assume
   reasoning, batch, Files, or Responses support.
2. **Hosted OpenAI** — a named backend using OpenAI Platform auth and API semantics. This
   is where OpenAI model defaults, reasoning params, Responses API evaluation, file inputs,
   and OpenAI Batch belong.

The existing `kind = "openai"` can keep carrying both only if hosted-only features are
gated by explicit backend/slot capability. If that becomes awkward, split a first-class
`openai-hosted` kind rather than weakening the local-compatible path.

## Work plan

### 1. Document the hosted OpenAI config

Add a short README/config example that is copy-pasteable:

```toml
[backends.gpt]
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
key_optional = false

[casts.gpt]
explorer = { backend = "gpt", id = "gpt-5.6-luna" }
synth = { backend = "gpt", id = "gpt-5.6-terra", vision = true }
```

Model IDs must be checked against current OpenAI docs when the implementation PR starts;
provider defaults drift.

### 2. Probe hosted OpenAI with the current implementation

With `OPENAI_API_KEY` set:

- `oneshot` text-only call on the hosted cast.
- `oneshot` with an image attachment on a `vision = true` synth.
- `consult` that calls `view_image`, verifying the user-turn rewrite reaches the model.
- Failure probe with `vision = false`, verifying kaibo refuses image input before a
  provider call.
- Trace check for `gen_ai.request.thinking`; expected today: absent for `openai`.

Record results in `docs/devlog.md` if they drive a change.

### 3. Add OpenAI reasoning shaping

Official current guidance for GPT-5.6 says to use the Responses API for reasoning,
tool-calling, and multi-turn workflows, and to set `reasoning.effort` intentionally.
OpenAI's API reference also exposes reasoning effort on modern response/chat shapes.

Implementation choices to settle with tests:

- If staying on rig's Chat Completions client, verify whether `additional_params` can
  safely carry `reasoning_effort` and the correct output-token field for current hosted
  GPT models.
- If moving hosted OpenAI to Responses, add a small direct client arm rather than forcing
  Responses semantics through the generic OpenAI-compatible path.
- Preserve kaibo's doctrine: reasoning on by default where the model supports it; explicit
  opt-out per slot; no silent "reasoning absent" for a reasoning-capable hosted model.

Test shape:

- `ModelShape::resolve(ProviderKind::Openai, "gpt-…")` emits the selected OpenAI reasoning
  params.
- `sinks_effort` / inert tunable rendering treat OpenAI reasoning-capable models as effort
  sinks.
- Non-reasoning/local OpenAI-compatible slots can remain no-reasoning by explicit shape or
  backend capability.

Sources:

- [Latest OpenAI model guidance](https://developers.openai.com/api/docs/guides/latest-model)
- [Responses streaming reference: reasoning](https://platform.openai.com/docs/api-reference/responses-streaming/response/refusal/delta?lang=curl)

### 4. Re-evaluate Responses API for hosted OpenAI

OpenAI's help recommends the Responses API unless it lacks a capability the older
Completions APIs provide. For kaibo, the decision hinges on whether rig's current OpenAI
provider preserves the features kaibo needs:

- tool calling over repeated turns;
- image input;
- model reasoning controls;
- output token caps with reasoning-token headroom;
- usage reporting, especially reasoning tokens;
- prompt caching controls if useful for long resident preambles.

If Responses wins, keep the direct client narrow and test it offline with scripted request
serialization before any live probe.

### 5. Add OpenAI Batch after interactive hosted OpenAI is correct

OpenAI Batch is file-based: upload JSONL, create a batch against an endpoint, poll, then
download output/error files. That is a different body and lifecycle from Anthropic's and
Gemini's current batch implementations.

Open questions:

- whether kaibo deletes OpenAI output files by default or leaves them for audit/debug;
- whether OpenAI batch starts on `/v1/responses`, `/v1/chat/completions`, or both;
- how image/file attachments are represented in JSONL without creating a model-steerable
  write path.

Source:

- [OpenAI Batch API reference](https://platform.openai.com/docs/api-reference/batch/object?api-mode=responses)

### 6. Security and privacy checks

- Keep API keys out of config values; use env var names or key files only.
- Document that OpenAI API data controls are Platform controls, not ChatGPT subscription
  controls.
- For image/file inputs, note OpenAI's endpoint data policy: the Platform docs say image
  and file inputs can be scanned for safety even when some retention controls are enabled.

Source:

- [OpenAI Platform data controls](https://platform.openai.com/docs/models/default-usage-policies-by-endpoint)
