# kaibo configuration

The reference manual for kaibo's configuration surface: every key, its default, and
what it does. Implemented in `src/config.rs`, tested in `tests/config.rs`. The code is
ground truth where this document and the code disagree.

**Related material**

| where | what |
|---|---|
| `docs/config.example.toml` | the copyable annotated template (also `kaibo://config/example`, `kaibo example-config`) |
| `kaibo://config` | the *resolved* runtime state of a running server (also `kaibo config`) |
| `kaibo://config/guide` | this document, embedded in the binary |
| `docs/casts.md` | the design record for the backends/casts split |

**Configuration is optional.** With no config file, kaibo runs on a built-in registry
of backends and casts. A missing file at the default path is not an error.

## The model: backends, roles, casts

```
ProviderKind = anthropic | deepseek | gemini | openrouter | openai   (completion wires)
             | stability | openai-images                             (media kinds)
Backend      = { name, kind, base_url?, key source, request_timeout }
Cast         = { name, role → ModelSlot }                  (freely spans backends)
ModelSlot    = "backend/model-id"  or  { backend, id, pins…, tunables… }
```

Each concept owns one idea:

- **backend** — a *connection*. Carries `kind` (the closed `ProviderKind` enum; it
  selects the client and the request shape), `base_url`, a key source, and
  `request_timeout`. Answers "how do I reach Gemini". Kinds divide into two classes:
  the completion wires (everything through rig's completion clients) and the media
  kinds (`stability`, `openai-images` — image-generation APIs with no completion
  surface).
- **role** — a *job* a model serves. Three exist: `explorer` and `synth`, the two
  reasoning phases, and `image`, the media member that staffs the `generate` tool.
  A reasoning role takes a completion backend; the `image` role takes a media backend
  (kind `stability` or `openai-images`) — pairing either with the wrong class is a
  load error naming the fix. Image *input* is a slot capability (the `vision` pin on
  a reasoning slot), not a role.
- **cast** — a *composition*. A named assignment of models to roles. This is what the
  `cast` call parameter selects.

**Selection rule.** Calls pick casts. Backends are reachable only through a cast's
slots: calls choose a composition, compositions choose connections. A slot reference
borrows its backend's connection and never resolves another cast, so reference chains
and cycles cannot be expressed.

### Backends: `[backends.<name>]`

Connection settings only. Models are never declared here.

| key | type | default | notes |
|---|---|---|---|
| `kind` | `anthropic` \| `deepseek` \| `gemini` \| `openrouter` \| `openai` \| `stability` \| `openai-images` | required on a new backend | closed enum; selects client + request shape (`stability` and `openai-images` are the media kinds — image slots only) |
| `base_url` | string | kind-dependent | required for a new `openai` backend; optional for `anthropic`/`gemini` and the media kinds; load error elsewhere |
| `wire` | `responses` \| `chat` | inferred | `kind = "openai"` only; load error elsewhere |
| `api_key_env` | env var *name* | seeded from `kind` | env source, checked first |
| `api_key_file` | path | seeded from `kind` | file source, checked second |
| `key_optional` | bool | `true` for `openai`, else `false` | allows a placeholder token |
| `data_collection` | `deny` \| `allow` | `deny` | `kind = "openrouter"` only; load error elsewhere |
| `request_timeout_secs` | integer > 0 | `[defaults]` value (900) | per-single-completion ceiling |

#### `kind`

Closed: adding a kind requires a client arm in code, not a config line. A new backend
must declare one, which seeds that kind's default key sources. Re-declaring an existing
backend with a *different* kind is a load error; a connection's protocol is not changed
by re-declaration.

#### `base_url`

- **`kind = "openai"`** — required on a new backend. This is what allows several
  openai-kind backends (hosted GPT, two local llama.cpp servers, an Ollama box) to be
  live at once under different names. The `OPENAI_BASE_URL` / local-default fallback
  belongs to the built-in `openai-local` backend alone, so a missing `base_url` is a
  load error rather than a silent dial of the wrong server.
- **`kind = "anthropic"` / `kind = "gemini"`** — optional. Unset resolves to rig's
  built-in `https://api.anthropic.com` / `https://generativelanguage.googleapis.com`.
  Set, it points that wire protocol at a compatible gateway or proxy.
- **`kind = "stability"`** — optional, same contract: unset dials
  `https://api.stability.ai`; set, it points the media wire at a compatible
  gateway or proxy.
- **`kind = "openai-images"`** — optional. Unset dials hosted OpenAI
  (`https://api.openai.com/v1`). Set, it points the same wire at any server speaking
  `/v1/images/generations` — a local stable-diffusion.cpp `sd-server`
  (`base_url = "http://localhost:1234/v1"`) is the expected local case. Key sources
  are shared with the `openai` kind (the same `OPENAI_API_KEY` / `~/.openai-key`),
  but the key is **required by default**: the default endpoint is hosted, so a
  keyless seed would send a placeholder bearer to a paid API and 401 on the first
  call instead of failing loudly at setup. A keyless local `sd-server` backend sets
  `key_optional = true` beside its local `base_url`. The `generate` tool's
  `output_format` field for this kind must be `png`, `jpeg`, or `webp` — checked
  before the call, since it names the stored artifact's on-disk format.
- **Every other kind** — a load error. rig fixes those endpoints.

The value is a root the client versions itself, never a full endpoint path. rig's
`ClientBuilder` appends the provider-specific path (Gemini's `/v1beta/models/...`),
the batch lane's `GeminiBatch` versions a configured host root with `/v1beta` the
same way, and the media clients append their own route (`/v2beta/...` for
`stability`; `/images/generations` for `openai-images`, so give that one a URL
through `/v1`).

#### `wire`

Picks the interactive request shape: `"responses"` for rig's Responses client, `"chat"`
for OpenAI-compatible Chat Completions. Normally unset — kaibo infers it from the
endpoint, giving OpenAI Platform's exact URL the Responses shape and everything else
Chat Completions. Hosted GPT and local Gemma both work without this knob.

Set it when the heuristic cannot see the truth: an OpenAI-compatible gateway that
implements the Responses API at `/v1/responses` but does not sit at Platform's URL.
Current GPT-5.x reasoning models require Responses — they reject Chat Completions'
`max_tokens` with `unsupported_parameter` — so `wire = "responses"` is what makes them
usable behind such a gateway. `wire = "chat"` is the symmetric opt-out, including on
Platform's own endpoint.

**Interactive-only.** `wire` never changes batch eligibility, which stays keyed to the
endpoint-exact check alone. A gateway proxying `/v1/responses` does not necessarily
proxy `/v1/batches` or the Files API.

#### Key resolution

A backend resolves its key from `api_key_env`, then `api_key_file`. Env wins.

**Secrets never appear inline in the TOML** — only the *name* of an env var or the
*path* to a key file. A config file should be safe to commit or paste.

`key_optional = true` substitutes a placeholder when no key is found, which is the
keyless local-server case. The placeholder fits the auth style: an empty query key for
Gemini, a non-empty bearer for header-auth backends (`src/credentials.rs`).

A key file that is *present but broken* (empty, unreadable, a directory) is a loud error
even on a keyless backend, because present-but-wrong is a mistake rather than "keyless".
Only a genuinely absent file falls back.

**Timing.** Keys resolve lazily, when the backend is first used to build a client, not at
config load. A missing or broken key on a backend no call touches never surfaces.

#### `kind = "openrouter"` specifics

OpenRouter is a keyed gateway rather than a wire protocol of its own. One
`OPENROUTER_API_KEY` reaches every upstream model family through a fixed endpoint.

- **Reasoning is on for every slot.** kaibo emits OpenRouter's unified
  `{"reasoning":{"effort":…}}` field. The gateway translates it into each upstream
  provider's native knob and drops it where the pinned model has none, so emitting it
  unconditionally is safe for non-reasoning models. The per-role `effort` reaches the
  gateway verbatim, and OpenRouter accepts all seven rungs on every model, normalizing
  each onto the upstream's own knob — so `xhigh`/`max` land here even on models that
  refuse them on that vendor's direct API (measured; see the ladder table under
  [`[defaults]`](#defaults)).
- **No `batch` lane.**
- **`data_collection` defaults to `deny`.** One slug routes across competing upstream
  hosts whose data policies differ, and kaibo's prompts carry your source, so
  no-collection routing is pinned on every request. A model whose only hosts collect
  (most `:free` variants) fails loudly instead of leaking quietly. `data_collection =
  "allow"` is the explicit opt-in: kaibo then emits no restriction and your OpenRouter
  account settings govern. `kaibo://config` renders the active policy per backend.

#### `request_timeout_secs`

Wall-clock ceiling on a *single* completion call. Default 900 (15 min), from
`[defaults]`. `0` is rejected at load, since it would time out every call instantly.

rig's prompt loop is non-streaming and has no native timeout, so without this a provider
that connects but never responds hangs the whole tool call. The setting is per-backend
because a slow local model legitimately wants a longer leash than a hosted API.

A non-streaming call cannot distinguish *wedged* from *slow but working*, so keep the
value above your slowest legitimate single completion.

#### Failure policy: no retry

kaibo does not retry a failed provider call. There is no backoff and no `max_retries`
knob. A 429/503/529 overload, a connection reset, a partial stream, or a backend that
hits `request_timeout` all fail the single completion, and `consult`/`oneshot` surface
that as a clean tool-result error (`is_error`) naming the cast and the underlying
detail.

The reasoning: a consult is an *optional* augmentation. The calling agent should read
the failure and proceed without the second opinion, or call again, rather than have its
own tool call fail at the protocol layer.

Failures are classified so the caller can pick a next step:

| class | examples | retry advice |
|---|---|---|
| transient | overload, rate limit, timeout, connection reset | a manual retry may succeed |
| non-transient | auth failure, bad request | fix the config or the call |
| kaibo-side | named as such | not a provider problem |

Classification is a heuristic over the provider's error *vocabulary*, not the HTTP
status, because rig surfaces the response body rather than the code. For a reliably slow
backend, raise its `request_timeout_secs`. Automatic retry and backoff belong in the
shared HTTP layer — rig ships an `ExponentialBackoff` wired only into its streaming path
today — and landing it for the non-streaming completion path is tracked as an upstream
contribution in `docs/issues.md`.

### Casts: `[casts.<name>]`

A role table. Each slot takes one of two forms:

- **String** — `"backend/model-id"`, the common case. The *first* `/` splits, so
  HuggingFace-style `org/model` ids keep their inner slash.
- **Table** — when the slot needs capability pins or per-slot tunables.

```toml
[casts.chimera]
explorer = "deepseek/deepseek-v4-flash"     # cheap fast sweeps
synth    = "claude/claude-sonnet-4-6"       # the model that answers

# table form: id + capability pins + per-slot tunables
# synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
# explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "..." }
```

**Roles.** `explorer`, `synth`, and `image`. A misspelled role, or a misspelled
per-slot key, is a load error rather than a silent no-op. The `image` slot is the
media member: it points at a media-kind backend and staffs the `generate` tool. Its
model id means what that kind says it means — for `stability` it picks the route
(`core`, `ultra`, or an SD3.5 variant); for `openai-images` it is the `model` field
(`gpt-image-1` hosted, or whatever a local sd-server loaded). It sends one
generation request, not a reasoning loop, so the reasoning tunables (`effort`,
`thinking_budget`, `temperature`, ...) written on it are inert — `kaibo://config`
flags them under `inert_tunables`.

A cast may omit a role. The interactive built-ins carry explorer + synth; the batch
built-ins carry `synth` only; none carries `image`. A user cast that omits a role is
valid config, and the tool needing the missing role fails at call time naming the gap
(`cast "lite" has no synth slot`). Absent means the capability is absent.

**Slot references.** An unknown backend in a slot is a load error naming the known
backends. An empty model id is rejected at load, since it would otherwise surface as an
unexplained provider 404 mid-call.

**`vision`** pins the slot's vision capability (whether it accepts image parts in model
context), overriding the built-in classifier. The classifier keys on the slot's backend
kind:

| kind | classifier default |
|---|---|
| `anthropic`, `gemini` | vision on |
| `deepseek` | vision off (text-only models) |
| `openai`, `openrouter` | vision off until pinned |

A generic endpoint is vision-off until its config says otherwise, because kaibo cannot
know what serves an arbitrary model id. `kaibo://config` reports the *resolved*
capability, not the raw config.

**Aliases.** Backends and casts both take a file-level `aliases = [...]` list. An alias
that collides with a real name at its level, or that two names both claim, is a load
error.

### Tunables: what lives where

| knob group | lives on | keys |
|---|---|---|
| connection | the **backend** | key source, `base_url`, `wire`, `request_timeout_secs` |
| model-tracking | the **slot** | `max_tokens`, `thinking_budget`, `temperature`, `effort`, `thinking_style`, `vision`, `preamble` |

A slot knob falls back to its per-role `[defaults]` value when omitted: `explorer` slots
inherit the `explorer_*` defaults, `synth` slots the `synth_*` side.

`preamble` is the exception — it has no `[defaults]` entry and its own fallback chain,
documented under [System prompts](#system-prompts-prompts).

### `[defaults]`

Global tunables every slot falls back to. Per-slot overrides are documented above;
`request_timeout_secs` seeds every backend.

| key | default | constraint |
|---|---|---|
| `max_tokens` | 16384 | must exceed `thinking_budget` on a budget-tier slot |
| `thinking_budget` | 8192 | — |
| `explorer_temperature` | 0.1 | `[0.0, 2.0]` |
| `synth_temperature` | 0.3 | `[0.0, 2.0]` |
| `top_p` | 0.95 | `(0.0, 1.0]` |
| `explorer_effort` / `synth_effort` | `"high"` | passthrough string |
| `thinking_style` | `"auto"` | `auto` \| `adaptive` \| `budget` |
| `request_timeout_secs` | 900 | > 0 |
| `call_deadline_secs` | 3600 | > 0 |
| `explorer_max_turns` | 100 | — |
| `synth_max_turns` | 200 | — |
| `session_capacity` | 128 | > 0 |
| `job_capacity` | 64 | > 0 |
| `inline_attach_budget` | 262144 (256 KiB) | `0` is legal |
| `max_attachments` | 32 | `0` disables the explorer's `attach` tool |

Out-of-range values are rejected at load, not clamped. This applies at the `[defaults]`
level and per slot.

**`max_tokens` / `thinking_budget`.** Output headroom and reasoning budget. Reasoning
bills against the completion budget, so `max_tokens` must sit well above
`thinking_budget`. On a slot whose model actually *sends* a budget (Anthropic's legacy
`budget_tokens` tier) an inverted pair is rejected at load on the slot's resolved
values, because Anthropic would 400 on it mid-call. A slot with no budget sink (Gemini
takes a `thinkingLevel`, Anthropic's adaptive tier an effort) carries an inert
`thinking_budget` and the pair is not checked. `kaibo models` (CLI) and the
`list_models` tool report each model's advertised output ceiling where the provider
publishes one; size a synth slot's `max_tokens` from that value.

**Sampling.** The explorer gathers exact citations and runs cold; the synth composes the
answer and gets slightly more room. Sent where a model accepts them: top-level for
DeepSeek and OpenAI, under `generationConfig` for Gemini. **Anthropic drops sampling
whenever thinking is on**, which is every Anthropic slot by default — the Messages API
400s on a custom `temperature` under thinking, and thinking is the higher-value default.

**`effort`.** Reasoning depth for models that take an effort parameter:

| kind | field |
|---|---|
| anthropic (adaptive tier) | `output_config.effort` |
| deepseek | `reasoning_effort` |
| gemini | `thinkingLevel` |
| openrouter | `{"reasoning":{"effort":…}}` |

Hosted OpenAI Platform reasoning models (`gpt-5*`) also take it, as `reasoning.effort` on
the Responses shape. Generic and local OpenAI-compatible endpoints on Chat Completions do
not, and neither does budget-tier Anthropic, which uses `thinking_budget` instead.

The value is a passthrough string, like a model id: **kaibo keeps no allowlist**, so a
rung a provider ships tomorrow works today.

**There is no universal ladder.** A `[defaults]` effort lands on every cast at once, so a
rung that suits one provider can be invalid on another — prefer a deep rung on the *slot*
that can use it. Measured 2026-08-01:

| provider | rungs |
|---|---|
| Gemini | `minimal` `low` `medium` `high` — Google's own schema rejects `none`/`xhigh`/`max`. `minimal` is the off-switch, itself model-dependent (`gemini-3.5-flash` takes it, `gemini-pro-latest` refuses it). |
| DeepSeek | all seven (`none` … `max`), strictly validated. `none` emits the structural `thinking:{"type":"disabled"}` — asking for zero effort while thinking stays enabled bills reasoning tokens anyway (probed: 160–253). |
| OpenRouter | all seven on everything; the gateway normalizes each onto the upstream's native knob, so a rung can reach a model that refuses it on that vendor's direct API. `none` emits the gateway's structural disable. |
| OpenAI (hosted) | **per model**, at both ends: `gpt-5.6` → `max`, `gpt-5.2` → `xhigh`, `gpt-5.1` → `high`; `gpt-5`'s bottom rung is `minimal` where 5.1+ use `none`. |
| Anthropic | the adaptive tier takes an effort; the budget tier (Haiku 4.5 and older) expresses depth as `budget_tokens` and has no effort field at all. **Which rungs the adaptive tier takes is still unmeasured** — see `docs/issues.md`. |

Every row above except the Anthropic one was measured against the live endpoint, and each
is re-checkable: `tests/consult.rs` carries an `#[ignore]`d probe per provider that fails
if a ladder moves, rather than letting this table quietly rot.

**rig's client can be a second ceiling on two wires**, independent of the provider: rig
parses kaibo's params into a typed struct for Gemini and for OpenAI's Responses API, and
a typed struct has a closed set of rungs. On rig 0.38 that made `max` fail for a hosted
GPT slot even though OpenAI's own API accepted it; **rig 0.41 added the rung, so `max`
now works there**. Gemini still stops at `high` — that one is Google's own limit, not
rig's. kaibo asks rig's converter before each call and refuses with a message naming the
cast, the slot, the backend and the rungs that wire *does* take, rather than letting a
bare ``unknown variant `max` `` surface mid-consult. That list is read back out of rig,
which is why the 0.41 upgrade widened it with no kaibo change.

**`"none"` is an off-switch, not the shallowest rung.** kaibo treats it as a sentinel
beside the ladder rather than a depth: where a provider ships a structural disable
(DeepSeek, OpenRouter) the request carries that rather than a zero-effort string, and the
batch lane's depth *floor* leaves it alone — a cheap bulk fan-out you turned reasoning
off for stays off instead of being lifted to `high` and billing thinking on every item.

**An effort with nowhere to land is said out loud.** Budget-tier Anthropic and the
generic OpenAI `/chat/completions` wire (every local llama.cpp / Ollama / gateway
backend) have no reasoning field, so the value is dropped. When *you wrote* the effort —
on a slot, in `[defaults]`, or via `KAIBO_*_EFFORT` — kaibo logs a startup warning naming
the cast and slot, and `kaibo://config` lists `effort` under that slot's
`inert_tunables`. The inherited built-in `"high"` stays quiet: every local cast inherits
it onto a toggle-less wire, so warning there would be noise on every ordinary setup.

**`thinking_style`.** Forces the Anthropic thinking shape instead of the built-in
classifier. `auto` picks adaptive for Opus 4.6+, Sonnet 4.6, and Fable 5, and
enabled-budget for older models plus Haiku 4.5. Set `adaptive` or `budget` when a new or
misclassified model ships. A no-op for non-Anthropic kinds; an unknown value is a load
error.

**`call_deadline_secs`.** Whole-call wall-clock ceiling on an interactive
`consult`/`explore`/`oneshot`, and the backstop for when the per-request
`request_timeout` does not fire (a stalled response body, a pooled keep-alive to a
wedged backend). Past it the call aborts with a clean tool-result error. Keep it above
the largest `request_timeout` a call can reach so it never cuts a legitimately slow
single completion; operators running a >30-minute local model should raise it.

It bounds `consult`, `explore`, `oneshot`, and async `consult_submit`. Two in-process
paths sit outside it by nature:

- **`deliberate`'s direct lane** is one long completion, bounded instead by its synth
  backend's `request_timeout` plus a small margin. A slow local `deliberate` gets its
  full patience without forcing the interactive ceiling up to hours.
- **The batch lane** holds no in-process wait at all. The work runs on the provider's
  queue and is collected by polling `job_get`.

**`explorer_max_turns` / `synth_max_turns`.** Available in `[defaults]` and per call
only. They bound the loop, not the model.

**`session_capacity` / `job_capacity`.** Both LRU, capacity-evicted, no TTL.
`session_capacity` caps multi-turn consult sessions held in memory. `job_capacity` caps
async-`consult` jobs (`consult_submit`), running plus finished-but-uncollected; evicting
a still-running job aborts it. It is smaller because a held job result is heavier than a
session's question/answer pair.

**`inline_attach_budget`.** Cumulative byte budget for inlining `consult` text
attachments into the driver prompt, consumed greedily in caller order. A text attachment
past the remaining budget is *demoted*: named in the prompt with a read-it-whole
directive instead of its bytes. Demotion is loud; nothing is dropped.

`0` inlines nothing, turning every text attachment into a directive. That is the escape
hatch for a small-context cast, such as a 4K-context local model that chokes on inlined
bytes a hosted model absorbs without trouble.

Inlined bytes ride every turn of the driver loop, so this bounds resident prompt cost,
not just one request. The toolless tools (`oneshot`, `batch_submit`) are unaffected;
with no shell to fall back on, they keep their own per-file and per-call caps.

**`max_attachments`.** Cap on how many files one explorer sweep may route with its
`attach` tool. The routed bytes ride alongside the sweep's report to whoever reads it —
the `consult` driver, or `deliberate`'s offline synth — without entering the explorer's
own context. Distinct from `inline_attach_budget`, which bounds inlining the *caller's*
attachments into the driver prompt: this bounds a sweep's own routing, and it is a
behavioral guard rather than a memory one (the per-file and cumulative byte caps in
`attach.rs` bound the worst case). `0` disables the tool. Also settable via
`KAIBO_MAX_ATTACHMENTS` and `--max-attachments`.

### Built-in registry (the defaults)

Five backends and five same-named single-backend casts ship in code, plus two batch
casts. This is why a missing config file is not an error.

| backend | kind | base_url | key env / file | aliases |
|---|---|---|---|---|
| `anthropic` | anthropic | — *(optional)* | `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt` | `claude` |
| `deepseek` | deepseek | — | `DEEPSEEK_API_KEY` / `~/.deepseek-key` | — |
| `gemini` | gemini | — *(optional, host root)* | `GEMINI_API_KEY` / `~/.gemini-api-key` | `google` |
| `openrouter` | openrouter | — *(fixed)* | `OPENROUTER_API_KEY` / `~/.openrouter-key` | — |
| `openai-local` | openai | `http://localhost:13305/api/v1` | `OPENAI_API_KEY` / `~/.openai-key` *(optional)* | `local`, `lemonade`, `gemma`, `gemma4` |

No built-in sets `wire`. It matters only for a new openai-kind backend whose endpoint is
not OpenAI Platform's own but should still take the Responses shape; see
`[backends.gateway]` in `docs/config.example.toml`.

| cast | explorer | synth | synth lane |
|---|---|---|---|
| `anthropic` | `anthropic/claude-haiku-4-5` | `anthropic/claude-sonnet-4-6` | |
| `deepseek` | `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-pro` | |
| `gemini` | `gemini/gemini-flash-lite-latest` | `gemini/gemini-3.5-flash` | |
| `openrouter` | `openrouter/qwen/qwen3.6-flash` | `openrouter/qwen/qwen3.7-max` | |
| `openai-local` | `openai-local/Gemma-4-E4B-it-GGUF` | `openai-local/Gemma-4-26B-A4B-it-GGUF` | |
| `gemini-batch` | — | `gemini/gemini-pro-latest` | `batch` |
| `anthropic-batch` | — | `anthropic/claude-opus-4-8` | `batch` |

**Why the `openrouter` built-in points at Qwen.** DeepSeek, Gemini, and Anthropic each
have their own keyed backend, so the gateway earns its place by reaching a family kaibo
cannot reach directly. OpenRouter serves no `~qwen/*-latest` router alias — only
anthropic, google, moonshotai, openai, and x-ai get those — so the built-in pins undated
family ids, which are the most rot-resistant Qwen ids available and track the newest
point release until the next `.x` bump.

The explorer `qwen/qwen3.6-flash` has `vision` pinned on: the classifier defaults an
`openrouter` slot to vision-off because the gateway fronts blind and sighted models
alike, but the flash is multimodal-in per OpenRouter's catalog. The synth
`qwen/qwen3.7-max` is text-only and takes no vision pin. To keep vision on the synth,
swap in `qwen/qwen3.7-plus` and pin its vision on — a weaker reasoner, roughly 4.5×
cheaper.

**Merging.** The TOML merges over this registry by name. Set one field on a built-in to
retarget it, or add new backends and casts. A slot's `lane` is sticky across a bare
re-declaration of its model, so retuning `gemini-batch`'s id leaves it on the batch lane.

**Reserved aliases.** Built-in alias names register at both levels — as cast aliases, so
`cast = "claude"` resolves, and as backend aliases, so a slot reference `claude/<id>`
resolves. Naming a new backend or cast after one is a collision error.

### Lanes

`lane` is a **per-slot** property, not a cast-level one.

| lane | meaning | staffs |
|---|---|---|
| *(unset)* | interactive | `consult`, `explore`, `oneshot`, `consult_submit` |
| `batch` | a provider batch API job | `batch_submit`, `deliberate` |
| `direct` | one long completion kaibo drives itself | `deliberate` |

Rules:

- The interactive *answering* tools — `consult`, `consult_submit`, `oneshot` — refuse a
  cast whose synth is on an offline lane, and `batch_submit` refuses a cast whose synth
  is not specifically `lane = "batch"`. A big offline-tuned model is never run
  interactively by accident, and the reverse.
- `explore` is exempt: it runs only the explorer arm, which is always interactive, so a
  deliberate or direct cast's explorer is valid there. It needs an `explorer` slot and
  nothing more.
- A `batch`-lane synth must sit on a batch-capable backend: Anthropic, Gemini, or a
  hosted OpenAI Platform backend. A local OpenAI-compatible server has no Batch API, so
  declaring `lane = "batch"` on a slot elsewhere is a load error.
- A lane on an `explorer` slot is a load error. The explorer always runs interactively.
- Because lane lives on the slot, a cast may pair an interactive explorer with an
  offline synth. That shape is what staffs `deliberate`. The built-in batch casts stay
  synth-only by choice, since batch is toolless and an explorer would be dead weight.
- `batch = true` at the cast level is backward-compatible sugar. It sets the synth
  slot's `lane = "batch"` and nothing else; there is one internal representation of lane.

`lane = "direct"` runs one long completion kaibo drives itself, with no async provider API
involved, for offline deliberation over a model too slow for a live tool loop. A big local
model is the intended case, but this is not enforced: unlike `batch`, the `direct` lane
applies no backend capability check, so any backend that resolves may carry one.

### Cross-backend casts

Each role on the backend that serves it best, selected under one name. Two extra
openai-kind connections and one composed cast:

```toml
[backends.gpt]
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
key_optional = false

[backends.llama]                # a second local llama.cpp server, keyless
kind = "openai"
base_url = "http://localhost:8080/v1"
key_optional = true

[casts.mixed]
explorer = "llama/qwen2.5-coder-7b"     # sweeps stay local and free
synth    = { backend = "gpt", id = "gpt-5.6-sol", vision = true, effort = "high" }
```

Every cast resolves the same way: each slot becomes an *arm*, with a client on the
slot's backend and a request shape fit to the slot's model and tunables. A cast whose
explorer and synth straddle a capability line, or sit on different kinds entirely, is fit
per arm by construction.

## Precedence and the three surfaces

Highest wins:

```
MCP per-call input  >  CLI flag  >  env var  >  config file  >  built-in default
```

**Per-call input** is the `cast`, `*_model`, `*_backend`, and `*_max_turns` tool
arguments. The config supplies the defaults those override.

**A per-call model override** sends the model id verbatim. An id containing `/`
(HuggingFace style) is still one id: it is never parsed for a backend, so an org prefix
matching a backend alias cannot silently retarget the call. The override swaps the id
within the configured slot and drops that slot's pins and tunables, which described the
configured model; the new id classifies fresh.

**A per-call backend override** (`explorer_backend` / `synth_backend` on consult,
`backend` on oneshot) retargets the slot to another backend. Aliases resolve, and it
works even on a role the cast does not carry.

Everything else follows one naming rule:

> config key `foo_bar`  ⇄  env `KAIBO_FOO_BAR`  ⇄  CLI `--foo-bar`

| setting | config key | env var | CLI flag |
|---|---|---|---|
| config file location | — | `KAIBO_CONFIG` | `--config <path>` |
| default root | `server.root` | `KAIBO_ROOT` | `--root` |
| additional allowed trees | `server.allow_paths` *(list)* | `KAIBO_ALLOW_PATHS` *(colon-separated)* | `--allow-path DIR` *(repeatable)* |
| infer the cwd as an allowed tree + default root | `server.infer_cwd` *(default true)* | `KAIBO_NO_CWD` *(disables)* | `--no-cwd` *(disables)* |
| default cast | `server.cast` | `KAIBO_CAST` | `--cast` |
| disable a tool | `server.tools.<t> = false` | `KAIBO_NO_<T>` | `--no-<t>` |
| log filter | `server.log` | `RUST_LOG` *(wins)* / `KAIBO_LOG` | — |
| explorer max turns | `defaults.explorer_max_turns` | `KAIBO_EXPLORER_MAX_TURNS` | *(per-call only)* |
| synth max turns | `defaults.synth_max_turns` | `KAIBO_SYNTH_MAX_TURNS` | *(per-call only)* |
| max output tokens | `defaults.max_tokens` *(per-slot override)* | `KAIBO_MAX_TOKENS` | — |
| thinking budget | `defaults.thinking_budget` *(per-slot override)* | `KAIBO_THINKING_BUDGET` | — |
| explorer temperature | `defaults.explorer_temperature` *(per-slot `temperature`)* | `KAIBO_EXPLORER_TEMPERATURE` | — |
| synth temperature | `defaults.synth_temperature` *(per-slot `temperature`)* | `KAIBO_SYNTH_TEMPERATURE` | — |
| nucleus top_p | `defaults.top_p` | `KAIBO_TOP_P` | — |
| explorer effort | `defaults.explorer_effort` *(per-slot `effort`)* | `KAIBO_EXPLORER_EFFORT` | — |
| synth effort | `defaults.synth_effort` *(per-slot `effort`)* | `KAIBO_SYNTH_EFFORT` | — |
| thinking style | `defaults.thinking_style` *(per-slot override)* | `KAIBO_THINKING_STYLE` | — |
| LLM request timeout (s) | `defaults.request_timeout_secs` *(per-backend override)* | `KAIBO_REQUEST_TIMEOUT_SECS` | — |
| whole-call deadline (s) | `defaults.call_deadline_secs` *(must be > 0; default 3600)* | `KAIBO_CALL_DEADLINE_SECS` | — |
| session cache size | `defaults.session_capacity` *(must be > 0)* | `KAIBO_SESSION_CAPACITY` | — |
| async job cache size | `defaults.job_capacity` *(must be > 0; default 64)* | `KAIBO_JOB_CAPACITY` | — |
| attach inline budget (bytes) | `defaults.inline_attach_budget` *(0 = never inline; default 262144)* | `KAIBO_INLINE_ATTACH_BUDGET` | — |
| explorer attach cap (count) | `defaults.max_attachments` *(0 = attach tool off; default 32)* | `KAIBO_MAX_ATTACHMENTS` | `--max-attachments N` |
| exec timeout (s) | `sandbox.exec_timeout_secs` | `KAIBO_EXEC_TIMEOUT_SECS` | — |
| output cap (bytes) | `sandbox.output_limit_bytes` | `KAIBO_OUTPUT_LIMIT_BYTES` | — |
| scratch cap (bytes) | `sandbox.scratch_limit_bytes` *(must be > 0; default 64 MB)* | `KAIBO_SCRATCH_LIMIT_BYTES` | — |
| disable extra builtins | `sandbox.disable_builtins` *(list; file-only)* | — | — |
| ignore files | `kaish.ignore.files` *(list; replaces `[".gitignore"]`; file-only)* | — | — |
| ignore defaults | `kaish.ignore.defaults` *(default true)* | — | — |
| auto-load nested .gitignore | `kaish.ignore.auto_gitignore` *(default true)* | — | — |
| global gitignore | `kaish.ignore.global_gitignore` *(default false)* | — | — |
| ignore scope | `kaish.ignore.scope` *(`"enforced"` \| `"advisory"`; default `"enforced"`)* | — | — |
| telemetry on/off | `telemetry.enabled` *(default false)* | `KAIBO_TELEMETRY_ENABLED` | — |
| OTLP traces endpoint | `telemetry.endpoint` | `KAIBO_TELEMETRY_ENDPOINT` | — |
| export timeout (s) | `telemetry.timeout_secs` *(must be > 0)* | `KAIBO_TELEMETRY_TIMEOUT_SECS` | — |
| trace service name | `telemetry.service_name` | `KAIBO_TELEMETRY_SERVICE_NAME` | — |
| export headers | `telemetry.headers` *(map; file-only — values are secrets)* | — | — |
| persistence on/off | `persistence.enabled` *(default true)* | `KAIBO_NO_PERSISTENCE` | `--no-persistence` |
| state-db path | `persistence.path` *(default `$XDG_STATE_HOME/kaibo/state.db`)* | `KAIBO_STATE_DB` | `--state-db FILE` |
| project house-rules files | `context.project_files` *(list; default `["AGENTS.md"]`)* | `KAIBO_PROJECT_FILES` *(colon-separated)* | `--project-context-file FILE` *(repeatable)* |
| user house-rules files | `context.user_files` *(list)* | `KAIBO_USER_FILES` *(colon-separated)* | `--user-context-file FILE` *(repeatable)* |
| explorer system prompt | `prompts.explorer` *(file-only — full replace)* | — | — |
| consult system prompt | `prompts.consult` *(file-only — full replace)* | — | — |
| oneshot system prompt | `prompts.oneshot` *(file-only — full replace)* | — | — |
| batch system prompt | `prompts.batch` *(file-only — full replace)* | — | — |

**Two exceptions to the naming rule:**

- **Provider key vars stay native.** `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`,
  `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, and `OPENAI_API_KEY` are not renamed to
  `KAIBO_*`, because people and CI expect those names. A backend points at one via
  `api_key_env`.
- **`OPENAI_BASE_URL` is kept** as a backward-compatible override for any openai-kind
  backend with no explicit `base_url`. New backends use the `base_url` config key.

`RUST_LOG` follows tracing's own convention and takes precedence. `KAIBO_LOG` and the
`server.log` config key set the same filter at lower precedence.

### Tombstones (the `provider` spellings)

The rename map ships as load errors, never silent reinterpretation:

| old spelling | what happens now |
|---|---|
| `[profiles.<name>]` | load error pointing at `[backends]` + `[casts]` and `docs/casts.md` |
| `server.provider` | unknown-field load error (`deny_unknown_fields`) |
| `KAIBO_PROVIDER` | load error naming `KAIBO_CAST` and `docs/casts.md` |
| `--provider` | rejected by clap (unknown flag) |
| call arg `provider` | unknown-field error (`deny_unknown_fields`) — the alias is gone |

The call-argument `provider` alias survived one cycle after the rename. serde drops
unknown fields, so without the alias a client still sending `provider` would have been
silently ignored into the default cast. That cycle is over: the alias is removed, and a
stale `provider` is now an invalid-params error like every other tombstone above.

## Tool gating

A tool clears **two** gates to be advertised: the `[server.tools]` flag (equivalently
`--no-<tool>` or `KAIBO_NO_<TOOL>`), and a configured cast that can **staff** it.

The eight flags are *capability* switches, not one per MCP tool. `consult` gates both
`consult` and `consult_submit`; `batch` gates `batch_submit`; the `job_*` verbs have no
flag of their own and follow whichever handle producers are live. `generate` clears a
third gate as well: the media CAS must be on (`[cas] enabled`), because an
artifact-producing tool needs somewhere to store artifacts.

A tool nothing can staff has its route removed rather than shipping unusable. The calling
agent never sees a tool whose every call would fail, and an unusable tool stops costing
resident tokens in every session.

Which cast shape staffs which tool:

| tool | needs |
|---|---|
| `consult`, `consult_submit`, `oneshot` | a cast whose synth answers **interactively** (no offline lane) |
| `explore` | a cast with an `explorer` slot |
| `batch_submit` | a cast whose synth runs on `lane = "batch"` (or the `batch = true` sugar) |
| `deliberate` | a cast with an `explorer` **and** an offline synth (`lane = "batch"` or `lane = "direct"`) |
| `generate` | a cast with an `image` slot (a media backend: kind `stability` or `openai-images`) — plus `[cas]` on |
| `job_get`, `job_cancel`, `job_list`, `job_wait` | at least one live handle *producer*; they follow whatever survives above |
| `run_kaish`, `list_models` | no cast at all; advertised whenever their flag is on |

**Default installs.** This affects two tools. No built-in cast carries an `image` slot,
so `generate` stays dark until you configure one (the image-slot example in
`docs/config.example.toml`). And no built-in cast pairs an explorer with an
offline synth — the two built-in offline casts, `anthropic-batch` and `gemini-batch`, are
synth-only — so `deliberate` is not advertised until you configure a cast carrying both
slots. The DELIBERATE casts section of `docs/config.example.toml` is the worked example.
Any of the three hosted batch providers serves, as does a big local model on the `direct`
lane, which needs no batch API.

The eligibility predicates live in one table in `src/server/mod.rs` (`CAST_ENUM_RULES`),
which also feeds each surviving tool's `cast` enum. The tools advertised and the casts they
offer are one computation, so they cannot disagree.

### Finding out why a tool is missing

Removing the route is right for the model's tool list and wrong for the operator, so the
reason is reported twice:

- **A startup warning** names the cast shape that would bring the tool back.
- **`kaibo://config`'s `[runtime]` section** lists `advertised_tools` (what the server
  serves) and `unstaffable_tools` (each held-back tool mapped to the same requirement
  text).

`unstaffable_tools` omits tools the operator switched off. "You disabled it" and "nothing
can run it" are different answers, and `[tools]` already reports the first.

## File location & loading

XDG, with explicit overrides:

```
$KAIBO_CONFIG                           # explicit path wins
--config <path>                         # ... or this
$XDG_CONFIG_HOME/kaibo/config.toml      # default
~/.config/kaibo/config.toml             # when XDG_CONFIG_HOME unset
```

Loading rules follow "crash rather than corrupt":

| condition | result |
|---|---|
| default XDG path absent | built-in defaults, no error |
| explicit `--config` / `KAIBO_CONFIG` path absent | hard error at startup |
| malformed TOML, or any validation failure below | hard error at startup, non-zero exit, before `serve()` |
| missing key for an unused backend | not fatal; keys resolve lazily at call time |

Validation failures that abort startup: an unknown key, including a misspelled role or
per-slot knob; a `base_url` on a keyed kind; an unknown backend in a slot; an empty
model id; an out-of-range sampling value; an inverted `thinking_budget`/`max_tokens`
pair on a thinking-kind slot; an alias collision; an unresolvable `server.cast`.

A setting the operator clearly meant is never silently dropped. A misspelled knob that
quietly does nothing is the failure mode these rules exist to prevent.

Startup validation of *which backends are usable* is tracked separately in
`docs/issues.md`. Project-local layering (a repo-root `.kaibo.toml` merged over the user
config) is a plausible later addition, not implemented.

## Telemetry (OpenTelemetry traces)

**Off by default.** kaibo reads a private codebase, and the spans `rig-core` emits carry
prompts, completions, and source snippets. A default run ships nothing off-box, so
`[telemetry]` is opt-in.

```toml
[telemetry]
enabled      = true                                # default false
endpoint     = "http://localhost:4318/v1/traces"   # OTLP/HTTP traces receiver
timeout_secs = 10                                  # per-export deadline; must be > 0
service_name = "kaibo"                             # service.name on the trace Resource
headers = { authorization = "Bearer <token>" }     # file-only; values are secrets
```

**What you get.** The GenAI trace tree rig produces — a tool call → `run_phase` per
phase → `invoke_agent` → a `chat` span per model turn, carrying `gen_ai.request.model`
and every `gen_ai.usage.*` token count. kaibo adds:

- The named parent spans (`consult`, `oneshot`, `run_kaish`) that root each trace.
- A `tool` span per tool invocation (`tool_span.rs`) carrying `gen_ai.tool.name` and an
  ok/err `outcome`, so a query can name *which* tool the model called (`run_kaish`,
  `view_image`, the nested `explore′`), not just that a turn happened. rig's own
  per-tool instrumentation is not reliably queryable across backends.

Transport is OTLP/HTTP with protobuf on the `/v1/traces` path, reusing kaibo's
`reqwest`. No gRPC and no second HTTP stack.

**Boundary.** Enabling opens an **outbound** OTLP connection to `endpoint`. This is
allowed under kaibo's stdio-only invariant: kaibo can read a filesystem, so it must never
*bind* a socket, but reaching out to a collector is not binding. Keep `endpoint` local
(the default `localhost:4318`) unless you intend to send traces, with full content, to a
remote.

Header **values** are secrets and never appear in the `kaibo://config` render; only the
header *names* do, like an API-key env-var name.

Logs continue to ride the `tracing` → stderr + MCP `notifications/message` path
regardless. Telemetry adds the traces signal only.

## Persistence: `[persistence]`

**On by default.** kaibo keeps a small state db so `consult` session threads and
provider batch handles survive a server restart and are shared across front doors — a
session started over MCP continues from the CLI. It lives at a fixed XDG state path,
never a path a model controls.

```toml
[persistence]
enabled = true                                  # default true
path    = "$XDG_STATE_HOME/kaibo/state.db"      # default; else ~/.local/state/kaibo/state.db
```

CLI/env: `--no-persistence` / `KAIBO_NO_PERSISTENCE` disable it (in-memory, like before);
`--state-db <FILE>` / `KAIBO_STATE_DB` move the db. `path` is `$VAR`/`~`-expanded like
`root`/`allow_paths`.

**Contents.**

| persists | never persists |
|---|---|
| the `(question, answer)` turns of each session, caller question included (capacity-evicted, no TTL, same as the in-memory store) | background consult/deliberate job handles (`job-N`), which are in-memory and session-only by design |
| the `{backend, provider-id, label}` of each submitted batch, so `job_list` can re-surface a handle after a restart | exploration reports, which would be stale bloat |

Stored content is the caller's questions, the models' answers, and batch-handle metadata.
Nothing the *inner model team* steers reaches disk: the store is handler-side, and kaish
has no write path to it. Because those answers are what you paid for, kaibo treats the db
as your data and never removes the file under any error; moving it aside is your decision.

**Read-only toward your project is unchanged.** The store is handler-side at the XDG
path. kaish's read-only sandbox never sees it, kaibo writes nothing into any project, and
`open` refuses a state-db path that resolves inside an allowed tree, so it cannot be
pointed into a repo. See the "Read-only is the product" invariant in AGENTS.md.

**Host-agent sandboxes.** kaibo's inner model-facing shell is read-only, but your MCP
client or calling agent may also sandbox the kaibo process itself.

- A host sandbox that blocks network prevents model-backed tools from reaching providers.
- A host sandbox that blocks writes to the XDG state dir fails a long-lived MCP server
  during startup when persistence is enabled.

Grant the host sandbox outbound network and a narrow writable root for kaibo's state dir,
or move the db with `--state-db` / `KAIBO_STATE_DB` / `[persistence] path`. In
multi-agent setups a per-client state db is often cleaner than sharing one history file
across Claude Code, Codex, and other agents. The same judgment applies to the media CAS
when an artifact-producing tool is enabled: the default XDG data path is convenient for
sharing generated artifacts across agents, while a per-client CAS keeps them separated.

**Failure is loud.** If the store cannot open — a bad path, a db inside a project, a
network mount (turso's multiprocess mode is 64-bit Unix plus local filesystem only) —
kaibo fails to start with an error naming the escape hatch, leaving the db untouched. It
does not drop to memory and lose your sessions at the next restart.

**One exception, Windows only.** On Windows and other non-64-bit-Unix targets the store is
single-process. A second kaibo opening the same db, such as a second editor window, would
crash-loop under an MCP client that auto-restarts its servers. So `SingleProcessLocked` is
the one carve-out: kaibo warns and serves with in-memory sessions for that run.

It is not silent. The startup log says so, and `kaibo://config` shows `persistence.active
= false` alongside `enabled = true`, so the calling model can see that durability is off.
Close the other kaibo, point `--state-db` elsewhere, or pass `--no-persistence` to make it
explicit. Every other open failure stays fatal.

## Media CAS: `[cas]`

**On by default, and it follows persistence.** The CAS is the content-addressed store an
artifact-producing tool writes into. Three produce today:

| producer | what it stores | opt-in |
|---|---|---|
| `generate` | images a provider rendered | a cast with an `image` slot |
| `save_artifact` | bulk text a consult's model wrote | `[artifacts] enabled` **and** a per-call `save_artifacts` (see below) |
| `deliberate` | the explorer dossier each deliberation was built on | none — the CAS being on is the only key |

`deliberate` needs no key of its own — kaibo writes the dossier, not a model. Each
deliberation names its dossier's `kaibo://cas/<digest>`; pass that digest back as the
`dossier` argument to run a second synth over the same evidence with no explorer sweep.
Size the store for dossiers: they are the bulkiest objects most installs hold, and they
accumulate on every deliberation. A refused write (a full `max_bytes`, an I/O error) is
logged and the deliberation proceeds — you lose the record, never the answer.

Its lifecycle has three states, reported as `mode` in `kaibo://config`'s `[cas]`
section:

| mode | when | what it means |
|---|---|---|
| `disk` | persistence is active | artifacts land at `dir`, durable across restarts |
| `memory` | persistence is off or degraded, or no `dir` resolves | artifacts are fetchable by digest for this run only; startup warns loudly |
| `off` | `enabled = false` | no store, and every tool that needs one is not advertised |

```toml
[cas]
enabled   = true                            # default true; false un-advertises the tools that need it
dir       = "$XDG_DATA_HOME/kaibo/cas"      # default; else ~/.local/share/kaibo/cas
max_bytes = 8589934592                      # optional soft cap; omit for no cap (the default)
```

CLI/env: `--cas-dir` / `KAIBO_CAS_DIR` move the store; `--cas-max-bytes` /
`KAIBO_CAS_MAX_BYTES` set the cap. `enabled` is file-only.

**The address is the content.** Every object's filename is the SHA-256 of its bytes, so
nothing (model or operator) can aim a write at a chosen path, and identical content is
stored once. Objects are written once and never rewritten, unlinked, or evicted — which
is why kaibo ships no GC: an object at a given digest stays that object forever, so no
address anyone holds goes stale.

Each object gets a `<hex>.json` provenance sidecar (prompt, model, cast, timestamp, mime,
seed, and which tool produced it). It makes an object self-describing **to whoever holds
its address**: one lookup by digest says what the bytes are and what format to serve them
as. Read it that way. The store is not built to be swept — a survey means walking 65,536
shards and parsing a file per object, and it only gets slower as the store fills, so kaibo
builds no index and offers no scan verb over it. Access is by address; a saved digest also
rides the session turn that produced it, so tracking what was created lives with the
conversation, not the store. An operator's cleanup, if wanted, is plain file mtime on the
object tree for now — see `docs/issues.md`.

The sidecar is **first-writer-wins**: it records the first write of a given content and is
never rewritten. Save bytes the store already holds and the record stays the original
one — which may name a different call, a different cast, or `generate`. It is metadata for
an operator, not an audit trail, and a content-addressed store that never rewrites cannot
be one. For a durable per-call record, kaibo's answers are the tool-call telemetry each
save emits and the session the answer was recorded into.

**Retrieval is operator-surface only.** Every producer names its artifacts by a
`kaibo://cas/<digest>` address, and two commands take the digest out of one and hand the
content back: the `read_cas` MCP tool, and `kaibo cas read` on the command line. The MCP
client or the CLI caller runs them; the inner model team never can — the CAS is not
mounted into kaish and no cast-facing tool reads it, because kaibo state spans projects
and a browsable CAS would let one project's team enumerate another's artifacts.

`read` is the only verb on either surface. There is no listing, no usage report, and no
delete — each needs an index this store does not keep. Prune by file mtime over the object
tree with your own tools.

The two surfaces serve the same object differently, because they write to different
places:

| | `read_cas` (MCP) | `kaibo cas read` (CLI) |
|---|---|---|
| metadata | leads as the first content block | goes to stderr, so stdout stays the payload |
| text content | returns a bounded window | goes to stdout as text |
| binary content | returns base64 only for an explicit range; returns a small image as a rendered block | goes to stdout as raw bytes — `kaibo cas read <digest> > arch.png` needs no flag |
| `--json` | not applicable | puts metadata and body in one stdout envelope, base64 for binary |

`kaibo cas read` runs in disk mode only. It refuses in memory mode and names the mode — a
memory store is empty in a new process, so every digest would otherwise read as "not
found".

Reads are **metadata-first and bounded**, and the default depends on what the object is:

| ask | text object | image | any other binary |
|---|---|---|---|
| `length: 0` | metadata only | metadata only | metadata only |
| no `length` | metadata + up to 64 KiB from `offset` | the whole image, viewable, when it is ≤ 5 MiB; otherwise metadata only | metadata only |
| `offset` + `length` | metadata + that range as text | metadata + that range as base64 | metadata + that range as base64 |

Metadata is always the first content block: digest, URI, mime, total bytes, binary or not,
the label when the object's record carries one, a `provenance:` line when it has no record
at all, the range served, and the real file path **in disk mode only** — memory mode has
no file, so there is nothing to point at. `length` is capped at 1 MiB; a larger ask is
refused rather than trimmed, so a caller's next `offset` is never wrong.

**A binary object is never dumped as base64 unless you ask for a range.** An image past
5 MiB comes back as metadata (plus the path, on disk) rather than ~7 MiB of base64 in your
context — open the file, or page it deliberately.

**Paging always advances.** A window that begins or ends inside a multi-byte character
comes back as base64 of exactly the bytes you asked for, with a note saying why, so the
served range still moves. Resuming at the range you were handed always terminates and
always reassembles the object byte for byte.

`kaibo://cas/<digest>` **was** an MCP resource until 2026-08-05 and no longer is. Two
reasons. Hosts treat resources as ambient context — some prefetch, some auto-attach —
which is the wrong posture for bytes a model just wrote; a tool call is deliberate, with
explicit arguments and a permission prompt. And `resources/read` is whole-blob with no
negotiation: a measured 3.8 MB PNG produced roughly 5 MB of base64 in one read. The URI
string survives as the artifact's name; only the resource route is gone.

**`max_bytes` refuses, never evicts.** A write that would pass the cap fails loudly and
nothing is deleted to make room. The cap is opt-in because enforcing it costs an
O(objects) walk of the store per new write. When a cap is set, *every* write is admitted
the same way, including one whose content the store already holds: exempting those would
let a full store answer "is this content already here?" by succeeding for held bytes and
refusing for new ones, across every project this kaibo has served.

**Disk mode warns when the disk is not really a disk.** On Linux, startup checks what
filesystem the CAS directory sits on. If it is overlayfs, tmpfs, or ramfs — the shape of a
container with no volume mounted — kaibo logs a severe warning and **proceeds**. It is not
a refusal and there is no acknowledgement flag: running on a throwaway filesystem on
purpose is legitimate, and what is not legitimate is finding out only after a generation
has been paid for. The warning names the filesystem and the directory; `kaibo://config`
reports the same finding under `[cas] backing`, and `kaibo config` prints it, so you can
check before spending anything rather than hunting through startup log after.

The fix is to mount a volume at the CAS directory, or point `[cas] dir` at one. A durable
filesystem produces no warning and no `backing` line. So does a check that cannot answer
(a non-Linux host, or a `statfs` that fails) — an unreadable filesystem type is not
evidence of anything, and a guard that spoke every time it failed to look would be tuned
out by the time it mattered.

**Failure is loud.** In disk mode, a structurally unusable CAS path fails startup with
an error naming the escape hatches: a dir that resolves inside an allowed project tree
(refused the same way the state db is), or a file sitting where the store or one of its
ancestors must be a directory. Write errors that only a real write can reveal
(permissions, disk full, a read-only mount) surface on the first generation instead —
opening the store writes nothing, so it cannot probe for them. Memory mode is the one
warned degrade, mirroring the persistence posture it follows.

## Saving artifacts: `[artifacts]`

**Off by default.** This is the only switch in kaibo whose default is off. Everything in
`[tools]` is advertised unless you disable it; this one is disabled unless you enable it,
because it is the only surface where a *model* decides that bytes become durable.

`save_artifact` is a tool injected into the `consult` driver's own toolset. The model
hands kaibo bulk content it wrote and gets back a digest; the answer's footer names each
`kaibo://cas/<digest>` and you decide whether to read it. The point is your context
window: a consult asked to produce a generated corpus, a long inventory, or a fixture no
longer has to spend the answer on material you may only want to store.

```toml
[artifacts]
enabled = false                             # default false; true grants standing permission
```

CLI/env: `--allow-save-artifact` (serve only) / `KAIBO_ARTIFACTS_ENABLED`. Unlike the
`KAIBO_NO_*` flags, the env var sets the knob either way and the CLI flag *enables* — the
built-in default is the conservative end of the range, so a layer that could only disable
would have nothing to say.

**Three conditions, all required.** The tool is absent from the model's toolset unless
every one holds:

| condition | surface | who decides |
|---|---|---|
| `[artifacts] enabled = true` | config / env / CLI | the operator, standing |
| `save_artifacts: true` on the call | the `consult` tool argument | the calling agent, per call |
| the media CAS is live | `[cas] enabled`, above | the operator |

A call that passes `save_artifacts` on a server missing either of the other two is
**refused**, with a message naming which one — never answered quietly without the
artifacts it asked for. `kaibo://config` reports `[artifacts] enabled`, so a caller can
see the server's posture before asking.

**Limits are fixed and not configurable.**

| limit | value |
|---|---|
| bytes per artifact | 1 MiB |
| artifacts per MCP call | 8 |
| bytes per MCP call | 8 MiB |
| label | one line, 200 bytes |
| formats | `text`, `jsonl`, `markdown` — a hint; anything else stores as text |

A save past a limit is **refused and stores nothing**; the refusal names the limit, the
actual size, and the way forward. The format is the one non-gate: the content arrives as
a JSON string, so it is UTF-8 text by construction — source code, a config, a log all
ship as `text` — and an unknown format name stores as `text/plain` with the coercion
stated in the tool result, never refused. Binary artifacts come only from `generate`. Nothing is ever truncated — a digest handed back for
content that is not what the model wrote would be silent corruption. The per-artifact
limit is a backstop rather than a working ceiling: the content rides in tool-call
arguments, so the model's own `max_tokens` binds first. The label is bounded and must be a
single line because it is rendered into the answer's footer, where a line break could
forge entries the caller would read as real artifacts.

**What the model cannot do.** It can write and it can never read. There is no list verb,
no read verb, and no `read_cas` in the inner toolset. The result of a save is a
digest and nothing else: it never reports whether the content was already in the store,
because that answer would let one project's model team probe another's artifacts. Refusals
are sanitized for the same reason — the model is told what to do next, never the store's
path or how full it is. Only the `consult` driver loop gets the tool; delegated explorer
sweeps never do.

**Where the digests are written down.** The answer's footer names every artifact this call
saved, with its mime, size, label, and (in disk mode) its path. A consult that saved and
then *failed* reports them too, in the failure text — the bytes are durable either way,
and a result without their addresses would strand them. A call carrying a `session_id`
also persists the footer with the answer, so the digests sit in kaibo's state db beside
the conversation that produced them.

One gap, stated plainly: **cancelling a running `consult_submit` job aborts it mid-flight,
so anything it had already saved is not reported.** Those artifacts exist and are readable
by digest, but the digests are gone unless the call also carried a `session_id`. Avoid it
by threading a session, or by letting a job finish.

The provenance sidecar beside each object is housekeeping metadata, first-writer-wins —
see the `[cas]` section above for what it does and does not claim. Retention is yours:
kaibo never prunes, and this tool makes minting cheap.

## House rules: `[context]`

kaibo's models work for other agents, so they benefit from inheriting the calling agent's
conventions. `[context]` names files whose contents are spliced into each consultation
tool's preamble (the system prompt) as standing guidance: an `AGENTS.md`, a shared user
guidance file, or whatever your project uses.

No filename is hardcoded in the product. The only default is the cross-tool `AGENTS.md`
convention, and that is a config default you can change or turn off.

```toml
[context]
# Root-relative, read IF PRESENT (absent is normal). Default: ["AGENTS.md"].
# An explicit [] opts out of even that.
project_files = ["AGENTS.md", "docs/CONVENTIONS.md"]

# Absolute/tilde paths, read UNCONDITIONALLY (a missing one is a startup-visible
# error — you declared it, so kaibo won't silently drop it). Default: none.
user_files = ["~/.config/kaibo/agent-guidance.md"]
```

Two lists with different failure semantics:

| list | paths | missing file |
|---|---|---|
| `project_files` | root-relative | normal; read if present |
| `user_files` | absolute or `~` | error when a call assembles its prompt |

Both are read when a consultation phase builds its preamble, not at config load, so a
broken `user_files` entry surfaces on the first call that needs it rather than at startup.

`project_files` are joined to the resolved project root and canonicalize-checked to stay
within it. A configured `../` or an out-of-tree symlink is refused, so the containment
that bounds the read-only shell also bounds what gets injected. A repo with no `AGENTS.md`
is the normal case.

`user_files` are read-required: you named the file on purpose, so a missing one is a loud
error rather than a silent skip that ships an answer without the guidance you counted on.

**Trust boundary.** `user_files` may sit outside the allowed set because these files are
read in trusted server-side Rust at the tool handler, at the same trust level as
`config.toml` itself, and only their *contents* reach the model, never the path. The
read-only kaish shell still cannot reach the user guidance path; the model's read scope is
not widened.

This is the distinction from `[server] allow_paths` below. `allow_paths` widens what the
*model* can explore; `[context]` injects fixed operator text the model never navigates to.

**Where it lands.** Every codebase-reading phase: the `consult` driver and its nested
`explore′` sweep, standalone `explore`, and `deliberate`'s dossier explorer. The cheap
explorer therefore orients on the same guidance while it searches, not only at answer
time. The toolless `oneshot` and the offline batch synth read no project and get none.

**Precedence** is the usual per-call > CLI > env > file > built-in. A CLI
`--project-context-file` replaces lower layers additively. The CLI cannot express "empty";
opt out with `[context] project_files = []` or `KAIBO_PROJECT_FILES=`.

## System prompts: `[prompts]`

`[context]` *adds* project guidance. `[prompts]` *replaces* the built-in role framing,
which is the system prompt each phase runs under. One override per phase:

```toml
[prompts]
explorer = "You are a security auditor. Hunt injection sinks and unsafe deserialization."

# Triple-quoted for multiline — the usual authoring shape.
consult = """
You are a staff engineer reviewing this codebase.
Prefer architectural answers; name the file:line that carries each claim.
"""
```

| key | replaces | runs in |
|---|---|---|
| `explorer` | `report_preamble` | the nested `explore′` sweep inside `consult` |
| `consult` | `consult_preamble` | the `consult` driver |
| `oneshot` | `oneshot_preamble` | the thin, toolless `oneshot` |
| `batch` | `batch_preamble` | the offline, max-thinking `batch_submit` |

**Full replace.** An override *is* the role framing, verbatim; kaibo does not re-wrap it.
This is safe because the kaish operating contract — how to drive the read-only shell, the
exit-code meanings, the `cat -n` and `grep -rn` idioms — rides the `run_kaish` tool
description independently, so the model keeps the shell contract even when you rewrite the
prose.

An override does drop the tuned role framing kaibo ships: the explorer's "report, don't
conclude", the synth's "trust a grounded citation, reach for more", and the
positive-framing discipline that weaker and local models depend on. That becomes yours to
own.

**Orthogonal to `[context]`.** House rules still append on top of an override.
`[prompts]` sets the role, `[context]` adds the project's conventions, and both land in
the final system prompt. Layering order is `override-or-built-in` → `+ house rules`.

**File-only and operator-only.** Multiline prose has no clean env or CLI form, the same
constraint `telemetry.headers` has, so overrides live only in `config.toml`. They are not
a per-call tool argument: a calling agent cannot inject a system prompt, only the operator
who owns the config can. An empty or whitespace-only override is a load error, since a
blank system prompt is never intended. Remove the key to fall back to the built-in.

### Per-model overrides (the slot `preamble`)

`[prompts]` is keyed by *phase*, meaning the job. A prompt can also be keyed by *model*,
because the same phase may run different models: a local Gemma explorer wants different
framing than a Claude Haiku one. kaibo's request shaping is already model-aware, and the
prose can be too. The per-model knob is `preamble` on the cast's **slot**, beside
`effort` and `thinking_style`:

```toml
[casts.local]
explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "You are a careful reader; quote exact lines." }
synth    = "anthropic/claude-sonnet-4-6"   # no per-model prompt; uses [prompts] or built-in
```

**Precedence, per phase:** `slot.preamble` → `[prompts].<phase>` → built-in. The slot is
most specific (this model in this cast) and wins, the same way a slot `effort` overrides
the `[defaults]` effort. Set neither and the built-in runs.

**One model, two synth jobs.** The synth slot's model runs both the `consult` driver and
the toolless `oneshot`, so its `preamble` feeds both. Each phase resolves under its own
key, so they remain independently overridable: identical by default, free to diverge by
setting `[prompts].consult` and `[prompts].oneshot` separately. Read `slot.preamble` as
"this model's voice" and the phase keys as "this job's framing". The explorer has one job,
so no ambiguity arises.

A per-call model override (a bare slot) carries no `preamble`. Overriding the model does
not drag the configured slot's framing along. The empty-value load error applies here too.

**The offline synth phases inherit it too.** A synth slot's `preamble` feeds `batch` and
`deliberate` alongside `consult` and `oneshot` (`Cast::resolved_prompts` in
`src/config.rs`). This is load-bearing rather than incidental: on a batch or deliberate
cast, the synth slot *is* the offline synth, so its voice has to reach the offline phase
or the slot preamble would do nothing on exactly the casts built for that lane.

Each phase still resolves under its own key, so `[prompts].batch` overrides the built-in
`batch_preamble` for the batch phase alone. To give the offline lane a different voice
from the interactive one on the same cast, set the phase key rather than the slot.

## Repo orientation: `[orientation]`

A static, computed-once file map injected into the exploring preamble, so a model starts
knowing the project's files instead of spending its first turns on `glob`, `ls`, and
`find` to discover the layout. This is the structure-first approach from Agentless and
Aider, with no model in the loop.

```toml
[orientation]
enabled = true               # default; set false to turn the map off
full_list_max_files = 256    # ≤ this → inject the full file list; above → directory map
tree_max_depth = 4           # how deep the fallback directory map descends
```

**How it is built.** The server runs the kernel's own `glob -a --json '**/*'`
server-side per `explore` and `consult` call. This is the same ignore-aware enumeration
the model's shell would get, on the same VFS under the same ignore rules, so the map
cannot disagree with what the explorer's own `glob` and `grep` see. `-a` includes hidden
config such as `.github/` and `.cargo/`; the ignore filter still drops `.git` and
`target`.

**Size gating.** Orientation is an enhancement — the model always has `glob`, `grep`, and
`explore′` — so its absence is never fatal and no call is refused for being large.

| repo size | injected |
|---|---|
| ≤ `full_list_max_files` | the complete file list |
| above it | a directory map: the same files folded into a depth-limited tree of `dir/  N files` lines, descending `tree_max_depth` levels |
| above it, and the directory map would itself exceed `full_list_max_files` lines | a short discover-as-you-go note naming the discovery tools |

In the directory map, files deeper than `tree_max_depth` stay counted at the deepest shown
directory. Names are traded for structure, and the model recovers them with `glob
'DIR/**/*'` or `grep -rn`. An oversized directory map that gets skipped is logged at
`warn`; an enumeration failure or an empty result degrades to no map without one.

`full_list_max_files = 0` is a load error, since it would refuse every repo; disable the
block instead. `tree_max_depth = 0` is a load error, since it would render an empty map.

**Scope.** The exploring phases: the `consult` driver and its nested `explore′` sweep,
standalone `explore`, and `deliberate`'s dossier explorer. The toolless `oneshot` reads no
project and gets no map. Like `[context]`, the block re-sends each turn, which the size
gate keeps bounded. Whether it erases discovery
turns in practice is measurable through the per-tool `tool` spans (see Telemetry).

## Path containment

**Always on.** Every tool call's `path` argument, or the default root when `path` is
omitted, is resolved with `std::fs::canonicalize` (expanding symlinks and collapsing
`..`) and then checked against the **allowed set**. A path that does not fall at or under
one of the allowed trees is `invalid_params`, naming the allowed trees and the three knobs
that widen them.

**The allowed set** is constructed at startup from the canonicalized `--root`, every
canonicalized `--allow-path`, and the canonicalized launch cwd — the last unless `--root`
named a project or `--no-cwd` opted out. MCP clients start stdio servers with cwd set to
the workspace, so the zero-config case scopes itself to the project with no operator
action.

**`--allow-path` is additive.** It widens the boundary and never narrows it. Adding one
does not cost you the cwd, because reaching one more tree should not evict the workspace
the question is about. `--root` behaves differently: naming it is choosing the project, so
the cwd is not added beside it.

To make the allowed set exactly what you named, use `--no-cwd` (`KAIBO_NO_CWD`, `[server]
infer_cwd = false`). Every call must then pass its own `path`.

The resolved allowed set is reported in three places: a startup log line, the `## Scope`
section of the server's MCP `instructions` (visible in every `initialize` response), and
`kaibo://config`.

**The default root** is what a call resolves to when it omits `path`. It is an explicit
`--root`, or, when none is set, the launch cwd, inferred. The common single-workspace case
therefore needs no `--root`: kaibo knows the workspace from its cwd and uses it for both
bounding and defaulting. The inferred case is labelled as such in the `## Scope` handshake
and at `kaibo://config` (`default_root_inferred`).

Only `--no-cwd` leaves you with no default root, and an omitted `path` is then a parameter
error rather than a guess.

**Widening the boundary:**

```toml
# config.toml
[server]
allow_paths = ["~/src", "/data/fixtures"]
```

```sh
# env — colon-separated like PATH
KAIBO_ALLOW_PATHS=~/src:/data/fixtures kaibo

# CLI — repeatable
kaibo --allow-path ~/src --allow-path /data/fixtures
```

A non-empty CLI `--allow-path` set replaces the env and file layers entirely, the same
precedence rule as `--root`. That is layer precedence, not narrowing: whichever layer wins
still sits alongside the inferred cwd. `--allow-path /` lifts all limits.

`--root` is not repeatable. It names *the* project a path-less call defaults to, and there
can be only one, so the parser refuses a second occurrence rather than picking silently.
`--allow-path` is the repeatable knob.

**Configure access once.** Putting your whole workspace tree in `allow_paths` (`["~/src"]`)
puts every project under it in bounds, and because the client's workspace cwd lands inside
that tree, kaibo infers it as the default root automatically. You then never pass `path`
per call.

**Path expansion.** In `root` and `allow_paths`, the file and env layers expand a leading
`~` to `$HOME`, and `$VAR` / `${VAR}` from the environment. The CLI relies on your shell's
expansion instead. Paths with no `~` or `$` are taken as written.

A variable that is unset, set but empty, or non-UTF-8 is a load error rather than a silent
gap that would misplace the boundary. The empty case matters because `$EMPTY/scratch`
collapses to `/scratch` and `$EMPTY/` to `/`, the whole filesystem.

Write `$$` for a literal `$`; a directory literally named `$foo` is written `$$foo`. A
stray `$` that begins no reference is an error, so a typo cannot slip through as a literal
segment.

**Reading a scratch or temp space.** kaibo reads only what is in the allowed set and never
writes anywhere, so to let it read artifacts a workflow drops in a temp dir (a diff, a
generated file, a log), add that dir to `allow_paths`. Use the env var rather than a
host-specific literal so it resolves on whatever machine kaibo runs on:

```toml
[server]
allow_paths = ["~/src", "$TMPDIR", "$XDG_RUNTIME_DIR/kaibo"]
```

`$TMPDIR` (POSIX) and `$XDG_RUNTIME_DIR` (XDG) land on the per-user scratch dir on macOS
and sandboxed Linux respectively, where a bare `/tmp` would be wrong. This is opt-in:
widening to a shared, world-writable space like `/tmp` is a real boundary move, even a
read-only one, so kaibo never adds it for you.

**When no default root exists.** If `--allow-path` names a tree that does not contain the
launch cwd and no `--root` is given, there is no default root. The cwd is outside the
boundary, so adopting it would point the default at a path containment rejects. An omitted
`path` then returns `invalid_params` ("no `path` provided and the server has no default
root …"). Pass an explicit `--root` inside an allowed tree to restore a default.

**Resolution.** `resolve_root` (`src/server.rs`) returns the canonicalized path, so the
kaish VFS mount target is always resolved. A nonexistent or non-directory entry in
`--root` or `--allow-path` is a construction error at startup.

### Following git worktrees

On by default. When a call's `path` misses the allowed set, kaibo admits it if it is a
linked git worktree of a repo already in the set. A feature branch checked out in a
sibling directory (`git worktree add ../proj-feature …`), including one created
mid-session, is reachable without touching `allow_paths`.

This is narrower than widening to the parent: `--allow-path ~/src` would grant read of
everything under it, while follow admits exactly the worktrees of an already-allowed repo
and nothing else.

kaibo resolves this by reading git's own link files — a worktree's `.git` file and the
repo's `.git/worktrees/<name>/{gitdir,commondir}` — never by running `git`, which is not
in the build (see [the sandbox probe runbook](sandbox-probes.md)).

Trust flows outward from the allowed repo only. kaibo enumerates the worktrees the
*allowed* repo's common git dir vouches for and admits a candidate only if it falls inside
one. It never consults the candidate's own `.git`, so a foreign directory with a forged
`gitdir:` pointer cannot admit itself. The check runs only on the containment-miss path;
a normal in-bounds call is untouched.

Turn it off to keep the boundary strictly static:

```toml
[server]
follow_worktrees = false
```

```sh
KAIBO_NO_FOLLOW_WORKTREES=1 kaibo      # env
kaibo --no-follow-worktrees            # CLI (can only disable, like --no-<tool>)
```

The worktrees currently followed are listed at `kaibo://config` under `[runtime]`
(see below), recomputed on each read so a mid-session worktree shows up without a
reconnect.

## kaibo://config

An MCP resource at `kaibo://config` (`application/toml`) exposing the server's resolved
runtime state. Read it before making calls to see the full picture.

| section | contents |
|---|---|
| `allowed_paths` | the canonicalized trees a per-call path must be at or under |
| `default_root` | the effective default root, explicit or inferred; `default_root_inferred` distinguishes them |
| `default_cast` | the cast used when a call omits `cast` |
| `runtime` | state computed at read time (see below) |
| `tools` | the **configured** flags — what the operator enabled, not what is served; see `runtime.advertised_tools` for the live surface |
| `sandbox` | exec timeout, output cap, scratch (`/` MemoryFs) cap, any extra disabled builtins |
| `kaish.ignore` | the resolved ignore policy the file-walking builtins honor: `files`, `defaults`, `auto_gitignore`, `global_gitignore`, `scope` |
| `defaults` | the global tunables every slot falls back to, rendered so per-slot values read as deltas |
| `backends` | each connection: kind, `base_url`, key source names, `key_optional`, `request_timeout_secs` |
| `backend_aliases` / `cast_aliases` | alias → canonical name, built-in and file-declared, covering every name a `cast` param, slot reference, or per-call backend override resolves |
| `casts` | each composition's slots as `model = "backend/id"`, with the resolved `vision` capability and only the per-slot tunables actually set |

**`runtime`** is kept distinct from the configured knobs so a reader can tell what kaibo
discovered from what the operator set. It carries `follow_worktrees` (the knob's effective
value), `followed_worktrees` (the git worktrees admitted beyond `allowed_paths` right now,
recomputed each read so a mid-session worktree appears without a reconnect), and the
`advertised_tools` / `unstaffable_tools` pair described under [Tool gating](#tool-gating).

**`backends.base_url`** renders the *resolved* value for the openai kind, with the env and
local-default fallback applied; the raw configured value, when set, for the anthropic and
gemini kinds; and nothing for every other kind.

**Secret-safety contract.** `kaibo://config` includes key *source metadata* — the env var
name and file path an operator configured — and never the resolved key value. Keys resolve
lazily at call time and are never cached in the `Config` struct, so the render function has
no field holding a secret.

The render destructures `Backend`, `ModelSlot`, `Defaults`, `ToolGating`, and
`SandboxConfig` exhaustively, so a new field is a compile error at the render site. That
makes rendering a field an explicit decision, subject to secret review, rather than a
silent omission.

`api_key_env` and `api_key_file` are included on purpose: an operator debugging a
missing-key error needs to see which source the backend points at.

## CLI mirrors

Every "help me set up models" surface has a CLI equivalent, for a caller with no MCP
client:

| MCP | CLI |
|---|---|
| `kaibo://config` | `kaibo config` |
| `kaibo://config/example` | `kaibo example-config` |
| `configure` prompt | `kaibo configure [goal]` |
