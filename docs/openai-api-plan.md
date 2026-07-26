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
- [Managing Billing Settings on ChatGPT Web and Platform](https://help.openai.com/en/articles/9039756-managing-your-work-in-the-api-platform-with-projects)
- [OpenAI API quickstart](https://platform.openai.com/docs/quickstart/make-your-first-api-request)

## Current state in kaibo

- `ProviderKind::Openai` still covers generic OpenAI-compatible endpoints. The built-in
  `openai-local` backend stays on rig's Chat Completions client so local llama.cpp/Ollama/
  LM Studio-style servers keep the permissive `/v1/chat/completions` shape.
- A backend whose kind is `openai` and whose resolved base URL is exactly
  `https://api.openai.com/v1` is treated as hosted OpenAI. That arm uses rig's Responses
  client for interactive calls (`oneshot`/`consult`) so current GPT models get the output
  token mapping and multimodal/tool-loop shape they expect.
- The built-in `openai-local` backend defaults to a local server URL and optional key;
  hosted OpenAI works by adding a new backend such as `gpt` with
  `base_url = "https://api.openai.com/v1"` and `key_optional = false`
  (`docs/config.md`, `docs/config.example.toml`).
- Live probes on 2026-07-25 confirmed the hosted path:
  - `gpt-5.6-sol` text `oneshot` passed through the Responses arm.
  - `gpt-5.6-sol` `oneshot --attach docs/brand/banner-teal.png` saw the PNG and answered
    `kaibo`.
  - `gpt-5.6-sol` `consult` answered an image question through the model loop and
    `view_image` rewrite path.
  - A `vision = false` synth refused the same image locally before any provider call.
  - `gpt-4.1-mini` remains usable on the hosted Responses seam with sampling and without
    `reasoning.effort`.
- Vision is opt-in for generic `openai` slots. A hosted multimodal GPT slot must set
  `vision = true` until kaibo has a trustworthy OpenAI model-capability classifier.
- `view_image` already handles the OpenAI transport mismatch: OpenAI-compatible wires do
  not carry image bytes in tool results, so kaibo rewrites viewed images onto a user image
  turn before resuming the model loop (`src/consult/shaping.rs`,
  `src/consult/engine.rs`).
- Hosted OpenAI shaping is model-aware but intentionally conservative:
  - GPT-5-family models receive `reasoning.effort` and no sampling knobs.
  - Known older chat families (`gpt-4*`, `gpt-3.5*`, `chatgpt-*`) keep sampling and do
    not receive `reasoning.effort`.
  - Unknown hosted OpenAI IDs get no extra params until probed or explicitly classified.
- OpenAI Batch is supported by OpenAI's API, including `/v1/responses`, but kaibo has not
  implemented that provider lane yet. The existing kaibo batch lane supports Anthropic and
  Gemini; OpenAI's file/JSONL batch lifecycle is tracked as the next provider.

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

## Shipped implementation slice

### 1. Hosted OpenAI config

```toml
[backends.gpt]
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
key_optional = false

[casts.gpt]
explorer = { backend = "gpt", id = "gpt-5.6-luna", vision = true, effort = "low" }
synth    = { backend = "gpt", id = "gpt-5.6-sol",  vision = true, effort = "high" }
```

The example pairs Sol with a fast/tool-capable Luna explorer for consult mode. OpenAI's
current guidance describes Sol as the flagship model, Terra as the balanced model, and Luna
as the efficient/high-volume model; all three are current GPT-5.6 Responses models with
reasoning, vision input, and tool support.

### 2. Responses shaping for current GPT

Official current guidance for GPT-5.6 says to use the Responses API for reasoning,
tool-calling, and multi-turn workflows, and to set `reasoning.effort` intentionally.

Implementation choices:

- Use rig's Responses client for exact hosted OpenAI Platform endpoints, leaving
  `openai-local` and other OpenAI-compatible gateways on Chat Completions.
- Keep reasoning on by default for GPT-5-family hosted slots through `reasoning.effort`.
- Suppress custom sampling for current GPT reasoning models because the live provider probe
  rejected `temperature` there.
- Keep sampling for known older hosted chat models, and suppress `reasoning.effort` because
  the live `gpt-4.1-mini` probe rejected it.

Tests pin:

- exact hosted endpoint detection;
- GPT-5.6 hosted arms using Responses params with `reasoning.effort`, no sampling, and no
  legacy output-token field in `additional_params`;
- GPT-4.1 hosted arms keeping sampling while omitting reasoning;
- `kaibo://config` marking per-slot `effort`/`temperature` no-ops by endpoint and model,
  not by provider alone.

Sources:

- [Latest OpenAI model guidance](https://developers.openai.com/api/docs/guides/latest-model)
- [OpenAI model catalog](https://developers.openai.com/api/docs/models)

## Remaining work plan

### 1. Add OpenAI Batch

OpenAI Batch is file-based: upload JSONL with purpose `batch`, create a batch against an
endpoint, poll, then download output/error files. OpenAI's Batch API supports
`/v1/responses`, `/v1/chat/completions`, `/v1/embeddings`, `/v1/completions`, and
`/v1/moderations`; GPT-5.6 Sol/Terra/Luna model pages list `v1/batch` as an endpoint.

That is a different body and lifecycle from Anthropic's and Gemini's current batch
implementations, so this should be a dedicated provider adapter, not a thin flag on the
interactive path.

The product shape should match the existing batch doctrine:

- `batch_submit` stays tool-less. Each OpenAI batch item is one prompt plus shared
  attachments, answered once from what it was handed. No kaish, no explorer, no tool loop.
- `deliberate` is how OpenAI batch gets codebase understanding. An interactive explorer
  builds the cited dossier first; the batch synth receives that dossier as text and reasons
  over it offline.
- Sol is a natural batch synth once the provider adapter exists. Pair it with a medium
  dossier builder for deliberate rather than trying to give the batch request tools.

Intended config shape after the adapter ships:

```toml
[casts.gpt-batch]
batch = true
synth = "gpt/gpt-5.6-sol"

[casts.gpt-deliberate]
explorer = { backend = "gpt", id = "gpt-5.6-terra", vision = true, effort = "medium" }
synth    = { backend = "gpt", id = "gpt-5.6-sol", lane = "batch" }
```

Open implementation questions:

- whether kaibo deletes OpenAI output files by default or leaves them for audit/debug;
- whether kaibo's first OpenAI batch adapter starts on `/v1/responses` only, or keeps a
  fallback `/v1/chat/completions` body for older models;
- how direct `batch_submit` image/file attachments are represented in OpenAI JSONL. For
  `deliberate`, the synth should not receive images or tools directly; the explorer's
  dossier is the handoff.

Source:

- [OpenAI Batch API reference](https://platform.openai.com/docs/api-reference/batch/object?api-mode=responses)

### 2. Probe optional OpenAI surfaces

- Codex-specialized model slugs exposed to some accounts: useful candidate for review casts,
  but do not promote to the canonical example until probed through kaibo's Responses arm.
- GPT-5.6 pro mode: possible fit for deliberate/deep review, but it changes latency and cost;
  add only with explicit config/tests and a live comparison.
- Persisted Responses/conversation state: likely not needed for kaibo's own bounded loops yet,
  and may complicate the read-only/account-state boundary.

### 3. Security and privacy checks

- Keep API keys out of config values; use env var names or key files only.
- Document that OpenAI API data controls are Platform controls, not ChatGPT subscription
  controls.
- For image/file inputs, note OpenAI's endpoint data policy: the Platform docs say image
  and file inputs can be scanned for safety even when some retention controls are enabled.

Source:

- [OpenAI Platform data controls](https://platform.openai.com/docs/models/default-usage-policies-by-endpoint)
