# kaibo configuration

Read `kaibo://config` or run `kaibo config` for resolved settings. Copy only the
sections you need from `kaibo example-config`; all configuration is optional.
A missing default config file uses built-in defaults.

| Need | MCP resource | CLI |
|---|---|---|
| Current settings | `kaibo://config` | `kaibo config` |
| Copyable TOML | `kaibo://config/example` | `kaibo example-config` |
| This reference | `kaibo://config/guide` | `kaibo config-guide` |
| Codex access and consent | `kaibo://config/codex` | `kaibo config-guide codex` |
| Guided setup | `configure` prompt | `kaibo configure [goal]` |

Hosted models receive your prompt, supplied context, and the source kaibo reads for
the call. Agree on projects and providers during setup. kaibo keeps the project
read-only; its own sessions and artifacts live outside the project.

## The model: backends, roles, casts

A **backend** is a named connection: protocol (`kind`), endpoint, and key source.
A **cast** assigns models to **slots**: `explorer`, `synth`, and optionally `image`.
Each slot names `"backend/model-id"` or a table with `backend`, `id`, and tunables.
Calls select a cast; slots reference backends, never other casts.

`explorer` and `synth` require completion backends. `image` requires a media backend.
Image input is the `vision` capability on a reasoning slot, not a separate role.
See `docs/casts.md` for the design rationale.

### Backends: `[backends.<name>]`

Connection settings only. Models are never declared here.

| key | type | default | notes |
|---|---|---|---|
| `kind` | `anthropic` \| `deepseek` \| `gemini` \| `openrouter` \| `openai` \| `stability` \| `openai-images` \| `gemini-images` \| `dashscope` \| `bfl` | required on a new backend | closed enum; selects client + request shape (`stability`, `openai-images`, `gemini-images`, `dashscope`, and `bfl` are the media kinds — image slots only) |
| `base_url` | string | kind-dependent | required for a new `openai` backend; optional for `anthropic`/`gemini` and the media kinds; load error elsewhere |
| `wire` | `responses` \| `chat` | inferred | `kind = "openai"` only; load error elsewhere |
| `api_key_env` | env var *name* | unset — declared by you | env source, checked first |
| `api_key_file` | path | unset — declared by you | file source, checked second (mutually exclusive with `api_key_cmd`) |
| `api_key_cmd` | argv array | unset — declared by you | command source: stdout, trimmed, is the key (no shell, 30s ceiling, stdin closed) |
| `key_optional` | bool | `true` for `openai`, else `false` | allows a placeholder token |
| `data_collection` | `deny` \| `allow` | `deny` | `kind = "openrouter"` only; load error elsewhere |
| `request_timeout_secs` | integer > 0 | `[defaults]` value (900) | per-single-completion ceiling |

#### `kind` and `base_url`

A new backend must declare `kind`. Re-declaring a built-in backend cannot change its
kind. No backend declares a key source unless you write one.

| kind | endpoint when omitted | custom `base_url` |
|---|---|---|
| `openai` | built-in `openai-local`: `http://localhost:13305/api/v1` | required on new backends; include `/v1` |
| `anthropic` | `https://api.anthropic.com` | host root |
| `gemini`, `gemini-images` | `https://generativelanguage.googleapis.com` | host root; kaibo adds `/v1beta` |
| `stability` | `https://api.stability.ai` | host root |
| `openai-images` | `https://api.openai.com/v1` | include `/v1`; kaibo adds `/images/generations` |
| `dashscope` | `https://dashscope-intl.aliyuncs.com` | host root; kaibo adds the multimodal route |
| `bfl` | `https://api.bfl.ai` | host root; kaibo adds the operation route |
| `deepseek`, `openrouter` | fixed provider endpoint | refused |

Separate names allow several connections using the same protocol. `OPENAI_BASE_URL`
overrides the built-in local backend when no explicit URL is present. A new `openai`
backend without a URL is refused, rather than redirected to the local default.

Media kinds staff only `image` slots. `gemini-images` uses `generateContent` with
image modalities and refuses `op`. `openai-images` accepts `png`, `jpeg`, or `webp`
output; a keyless local server must explicitly set `key_optional = true`.
DashScope text models use a separate `openai` backend at `/compatible-mode/v1`.
DashScope and BFL fetch generated media from signed URLs. BFL also polls the returned
regional `polling_url` and may return a `job-N` handle for a long generation.
BFL model IDs name operations: `flux-dev`, `flux-2-pro`, `flux-2-flex`,
`flux-pro-1.1-ultra`, or `flux-kontext-pro`.

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

```toml
[backends.deepseek]
api_key_file = "~/.deepseek-key"
```

Every backend needs a declared key source unless it is intentionally keyless:

| source | value in config | behavior |
|---|---|---|
| `api_key_env` | environment variable name | checked first |
| `api_key_file` | file path | checked after env; exclusive with `api_key_cmd` |
| `api_key_cmd` | command argv | checked after env; exclusive with `api_key_file` |

No conventional env variable or dotfile is read automatically. Existing installations
that relied on implicit sources must declare them. Store values in the environment,
file, or vault; config carries only references. Keys resolve afresh when each model
client is built, not when config loads. A two-role call can run a key command twice.
Unused backends do not resolve keys. Rotate a file or vault value without restarting;
restart an MCP server to change its config or inherited environment.

`api_key_cmd = ["op", "read", "op://Vault/Item/Field"]` runs argv with no shell and no
`$VAR` or `~` expansion. It inherits kaibo's environment, closes stdin, and trims
stdout to obtain the key. The command must finish within 30 seconds. Empty,
non-UTF-8, oversized, nonzero-exit, or timed-out output is refused. Output is never
logged; errors report status and stderr byte count, not its content. Put pipelines
in an operator-owned script and name that script.

`key_optional = true` permits a placeholder when a source is absent. A present but
broken source still fails, including an empty or unreadable file or failing command.
A missing-key diagnostic needs source metadata, never the secret itself.

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

#### Failure policy

There are no retry settings. kaibo retries a malformed model tool call twice. The
completion wrapper also retries selected transient provider failures up to four
times with bounded backoff: HTTP 429, 500, 502, 503, and 529, or recognized overload
and rate-limit text when no status survives. Other errors return to the caller.

Connection failures do not establish a provider rejection. Check the endpoint and
host access, including DNS, proxy, TLS, and network permissions. For Codex, read
`kaibo://config/codex` or run `kaibo config-guide codex`. Continue within the user's
authorized scope; an error is not consent to send data elsewhere.

`request_timeout_secs` bounds each request; `call_deadline_secs` bounds interactive
work including retries. Error results preserve the cast and underlying detail.

### Casts: `[casts.<name>]`

A role table. Each slot takes one of two forms:

- **String** — `"backend/model-id"`, the common case. The *first* `/` splits, so
  HuggingFace-style `org/model` ids keep their inner slash.
- **Table** — when the slot needs capability pins or per-slot tunables.

```toml
[casts.chimera]
explorer = "deepseek/deepseek-flash"        # reads the tree and cites
synth    = "claude/claude-sonnet-4-6"       # the model that answers

# table form: id + capability pins + per-slot tunables
# synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
# explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "..." }
```

**Roles.** `explorer`, `synth`, and `image`. A misspelled role, or a misspelled
per-slot key, is a load error rather than a silent no-op. The `image` slot is the
media member: it points at a media-kind backend and staffs the `generate` tool. Its
model id means what that kind says it means — for `stability` it picks the route
(`core`, `ultra`, or an SD3.5 variant); for `gemini-images` it is the model in the request path; for `openai-images` it is the `model` field
(`gpt-image-1` hosted, or whatever a local sd-server loaded); for `dashscope` it is
the `model` field too (`wan2.6-t2i`); for `bfl` it names one of five operations
(`flux-2-pro`, ...) and doubles as `generate`'s default `op`. It sends one
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
| `thinking_style` | `"auto"` | `auto` \| `adaptive` \| `budget` \| `off` |
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
| openai (Chat Completions) | `reasoning_effort` |
| openai (Responses) | `reasoning.effort` |

OpenAI-compatible Chat Completions requests carry the bare `reasoning_effort` field.
Responses requests use `reasoning.effort`. Budget-tier Anthropic uses
`thinking_budget` instead. A server that rejects the field needs
`thinking_style = "off"`; kaibo preserves its error for diagnosis.

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
| Anthropic | the adaptive tier takes an effort; the budget tier (Haiku 4.5 and older) expresses depth as `budget_tokens` and has no effort field at all. **Which rungs the adaptive tier takes is still unmeasured.** |

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

**Inert effort.** Budget-tier Anthropic and slots with `thinking_style = "off"`
have no effort field. If you explicitly set effort on such a slot, in `[defaults]`,
or through `KAIBO_*_EFFORT`, kaibo warns at startup and reports `effort` under
`inert_tunables` in `kaibo://config`. An inherited built-in effort stays quiet.

**`thinking_style`.** Forces the thinking shape instead of the built-in classifier.
`auto` picks adaptive for Opus 4.6+, Sonnet 4.6, and Fable 5, and enabled-budget for
older models plus Haiku 4.5. Set `adaptive` or `budget` when a new or misclassified
Anthropic model ships; those two are no-ops for other kinds. An unknown value is a load
error.

`off` is different: it applies to **every** provider and sends no reasoning parameter at
all. Reasoning is on by default wherever a model can do it, because a thinking model
asked to answer thin gives noticeably worse results — so `off` is the deliberate opt-out,
not a tuning knob.

Reach for it when a server **rejects** a parameter rather than ignoring it. Most
OpenAI-compatible servers drop fields they do not know, and kaibo sends the portable
`reasoning_effort` spelling for exactly that reason; but a strict gateway validates the
whole body and refuses. Crusoe answers `403 Request blocked: parameter 'X' is not
allowed`. That failure is loud and names the parameter, and kaibo passes the provider's
message through to the calling agent, so the fix is one config line rather than a
mystery. Note the rungs are per **model**, not per gateway — a provider may take
`max` on one model and cap at `high` on another, and kaibo keeps no allowlist, so a
rung a model does not take comes back as that provider's own error naming what it
does take.

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

**`explorer_max_turns` / `synth_max_turns`.** Server-side only — `[defaults]` and the
env vars, no tool argument and no CLI flag. They bound the loop, not the model, and a
loop that reaches its cap still answers: kaibo runs one final tools-forbidden turn so the
caller gets the work, not a stall. Set them for the server so two calls to it stay
comparable.

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

**`max_attachments`.** Cap on how many files one explorer survey may route with its
`attach` tool. The routed bytes ride alongside the survey's report to whoever reads it —
the `consult` driver, or `deliberate`'s offline synth — without entering the explorer's
own context. Distinct from `inline_attach_budget`, which bounds inlining the *caller's*
attachments into the driver prompt: this bounds a survey's own routing, and it is a
behavioral guard rather than a memory one (the per-file and cumulative byte caps in
`attach.rs` bound the worst case). `0` disables the tool. Also settable via
`KAIBO_MAX_ATTACHMENTS` and `--max-attachments`.

### Built-in registry (the defaults)

Five backends and five same-named single-backend casts ship in code, plus two batch
casts. This is why a missing config file is not an error.

| backend | kind | base_url | conventional key env / file (declare to use) | aliases |
|---|---|---|---|---|
| `anthropic` | anthropic | — *(optional)* | `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt` | `claude` |
| `deepseek` | deepseek | — | `DEEPSEEK_API_KEY` / `~/.deepseek-key` | — |
| `gemini` | gemini | — *(optional, host root)* | `GEMINI_API_KEY` / `~/.gemini-api-key` | `google` |
| `openrouter` | openrouter | — *(fixed)* | `OPENROUTER_API_KEY` / `~/.openrouter-key` | — |
| `openai-local` | openai | `http://localhost:13305/api/v1` | `OPENAI_API_KEY` / `~/.openai-key` *(optional)* | `local`, `lemonade`, `gemma`, `gemma4` |

None of these are seeded (see Key resolution above): a built-in backend has no key
source until you declare `api_key_env`/`api_key_file` naming one of these values, or
`api_key_cmd` for a vault command instead.

No built-in sets `wire`. It matters only for a new openai-kind backend whose endpoint is
not OpenAI Platform's own but should still take the Responses shape; see
`[backends.gateway]` in `docs/config.example.toml`.

| cast | explorer | synth | synth lane |
|---|---|---|---|
| `anthropic` | `anthropic/claude-haiku-4-5` | `anthropic/claude-sonnet-4-6` | |
| `deepseek` | `deepseek/deepseek-flash` | `deepseek/deepseek-flash` | |
| `gemini` | `gemini/gemini-flash-lite-latest` | `gemini/gemini-3.5-flash` | |
| `openrouter` | `openrouter/qwen/qwen3.6-flash` | `openrouter/qwen/qwen3.7-max` | |
| `openai-local` | `openai-local/Gemma-4-E4B-it-GGUF` | `openai-local/Gemma-4-26B-A4B-it-GGUF` | |
| `gemini-batch` | — | `gemini/gemini-pro-latest` | `batch` |
| `anthropic-batch` | — | `anthropic/claude-opus-4-8` | `batch` |

The built-in OpenRouter explorer has `vision = true`; its synth is text-only.
Use the live model catalog when changing model IDs or capabilities.

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
explorer = "llama/qwen2.5-coder-7b"     # surveys stay local and free
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

**Per-call input** is the `cast`, `*_model`, and `*_backend` tool arguments. The config
supplies the defaults those override. Turn limits are deliberately not among them.

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
| explorer max turns | `defaults.explorer_max_turns` | `KAIBO_EXPLORER_MAX_TURNS` | — |
| synth max turns | `defaults.synth_max_turns` | `KAIBO_SYNTH_MAX_TURNS` | — |
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
| traces signal on/off | `telemetry.traces` *(default true when enabled)* | `KAIBO_TELEMETRY_TRACES` | — |
| logs signal on/off | `telemetry.logs` *(default true when enabled)* | `KAIBO_TELEMETRY_LOGS` | — |
| OTLP logs endpoint | `telemetry.logs_endpoint` *(derived from `endpoint` when omitted)* | `KAIBO_TELEMETRY_LOGS_ENDPOINT` | — |
| metrics signal on/off | `telemetry.metrics` *(default true when enabled)* | `KAIBO_TELEMETRY_METRICS` | — |
| OTLP metrics endpoint | `telemetry.metrics_endpoint` *(derived from `endpoint` when omitted)* | `KAIBO_TELEMETRY_METRICS_ENDPOINT` | — |
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
  `KAIBO_*`, because people and CI expect those names. A backend points at one by
  declaring `api_key_env` in its stanza (kaibo reads no key env var that isn't
  declared).
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

## Tool gating

A tool clears **two** gates to be advertised: the `[server.tools]` flag (equivalently
`--no-<tool>` or `KAIBO_NO_<TOOL>`), and a configured cast that can **staff** it.

The eight flags are *capability* switches, not one per MCP tool. `consult` gates both
`consult` and `consult_submit`; `batch` gates `batch_submit`; the `job_*` verbs have no
flag of their own and follow whichever handle producers are live. `generate` clears a
third gate as well: the media CAS must be on (`[cas] enabled`), because an
artifact-producing tool needs somewhere to store artifacts.

A tool with no usable cast is omitted. The same eligibility rules filter its `cast`
argument, so advertised tools and selectable casts agree.

Which cast shape staffs which tool:

| tool | needs |
|---|---|
| `consult`, `consult_submit`, `oneshot` | a cast whose synth answers **interactively** (no offline lane) |
| `explore` | a cast with an `explorer` slot |
| `batch_submit` | a cast whose synth runs on `lane = "batch"` (or the `batch = true` sugar) |
| `deliberate` | a cast with an `explorer` **and** an offline synth (`lane = "batch"` or `lane = "direct"`) |
| `generate` | a cast with an `image` slot (a media backend: kind `stability`, `openai-images`, `gemini-images`, `dashscope`, or `bfl`) — plus `[cas]` on |
| `job_get`, `job_cancel`, `job_list`, `job_wait` | at least one live handle *producer*; they follow whatever survives above |
| `run_kaish`, `list_models` | no cast at all; advertised whenever their flag is on |

No built-in cast has an image slot or pairs an explorer with an offline synth, so
`generate` and `deliberate` need a configured cast before they appear.

### Finding out why a tool is missing

Read `runtime.advertised_tools` and `runtime.unstaffable_tools` in `kaibo://config`.
The latter names the cast requirements for missing tools. It omits tools you disabled;
those are visible under `[tools]`. Startup warnings report missing cast shapes too.

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

Unknown fields, invalid values, alias collisions, unknown backend references, empty
model IDs, and invalid lane/backend combinations fail at load. Budget-tier models
also require `max_tokens > thinking_budget` on resolved slot values.

Keys resolve when clients are built. The startup roster checks declared sources but
does not establish provider reachability. There is no project-local `.kaibo.toml`
layer; select another file with `--config`.

## Telemetry (OpenTelemetry traces, logs, and metrics)

`enabled` controls export. `traces`, `logs`, and `metrics` each default to true when
export is enabled. `capture_content` defaults to false; model IDs, token counts,
durations, finish reasons, and exit codes remain visible without prompt content.
An ambient OTLP endpoint enables telemetry unless explicitly disabled.

```toml
[telemetry]
enabled = true
endpoint = "http://localhost:4318/v1/traces"
traces = true
logs = true
metrics = true
# logs_endpoint = "http://localhost:4318/v1/logs"
# metrics_endpoint = "http://localhost:4318/v1/metrics"
timeout_secs = 10
service_name = "kaibo"
capture_content = false
# capture = ["gen_ai.output.messages"]
# headers = { authorization = "Bearer <token>" } # values are secrets
```

Omitted logs and metrics endpoints are derived from a traces endpoint ending in
`/v1/traces`. Otherwise set the exact endpoint or disable that signal; kaibo refuses
to guess. Transport is OTLP/HTTP protobuf.

### The standard environment

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | kaibo appends `/v1/traces`, `/v1/logs`, and `/v1/metrics` to this value — it is a root, not a full URL. Setting it turns telemetry on. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | kaibo posts spans to this value unchanged, and ignores the root above for spans. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | kaibo posts records to this value unchanged. |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | kaibo posts measurements to this value unchanged. Setting it alone also turns telemetry on, for a platform that collects metrics and not traces. |
| `OTEL_SERVICE_NAME` | sets `service.name` for traces, logs, and metrics. |
| `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` | kaibo exports content when this is set — it is the GenAI conventions' own name for the opt-in, read here unchanged. |
| `OTEL_SDK_DISABLED` | kaibo exports nothing, whatever any other source says. |

**Precedence: `KAIBO_TELEMETRY_*` / CLI > `config.toml` > `OTEL_*` > off.** This one
family inverts kaibo's usual env-beats-file rule, deliberately. `OTEL_*` is *ambient* —
a platform sets it for its own collector, not necessarily aiming it at kaibo — so it may
supply what you left blank and never override what you wrote. An explicit
`enabled = false` is absolute. `OTEL_SDK_DISABLED` is the one exception that beats
everything, because a kill switch something else can override is not a kill switch.

### Content and signals

An export allowlist filters span attributes, event attributes, and error descriptions.
Unknown attributes are dropped. Span names, event names, and timestamps remain.
`capture` admits named content attributes individually; `capture_content = true`
exports all content. Review the destination before enabling either. The startup log
and `kaibo://config` report `content_policy` and capture settings.

Logs export kaibo's own events, including model-loop diagnostics; rig's duplicate
content events are excluded. `logs = false` disables that signal. Metrics report
usage and behavior across calls without prompt or response content.

Traces include a phase per `run_phase`, provider `chat` spans, and `tool` spans with
`gen_ai.tool.name` and an `outcome`. Metrics can stay enabled with traces disabled.

kaibo emits six instruments, all histograms, all named by the conventions:

| Metric | What it answers |
|---|---|
| `gen_ai.client.token.usage` | Spend, split by `gen_ai.token.type` (`input` / `output`) and by model |
| `gen_ai.client.operation.duration` | How long one provider call took, including calls that failed |
| `gen_ai.invoke_agent.duration` | How long a whole phase took, end to end |
| `gen_ai.invoke_agent.inference_calls` | How many turns a phase used — a phase pinned at its turn cap shows as pile-up |
| `gen_ai.invoke_agent.tool_calls` | **Did the consult driver delegate**, or read everything itself |
| `gen_ai.execute_tool.duration` | How long one `run_kaish` / `explore′` / `view_image` took |

Agent metrics carry `gen_ai.agent.name`: `synth` or `explorer`. A delegated survey
counts as an explorer invocation; the synth's tool-call count includes its delegation.
Histogram buckets follow the GenAI conventions.

If a nonstandard traces endpoint prevents metrics-endpoint derivation, an omitted
`metrics` setting warns and skips metrics. Explicit `metrics = true` makes that case
a startup error. Set `metrics_endpoint` to resolve it.

Export uses outbound connections. Choose a local collector unless remote export is
intended; content export needs its own opt-in. Header values never appear in
`kaibo://config`, only names. stderr and MCP `notifications/message` continue
independently of OTLP export.

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

**One exception, single-process targets.** On Windows and other non-64-bit-Unix targets the store is
single-process. A second kaibo opening the same db, such as a second editor window, would
crash-loop under an MCP client that auto-restarts its servers. So `SingleProcessLocked` is
the one carve-out: kaibo warns and serves with in-memory sessions for that run.

It is not silent. The startup log says so, and `kaibo://config` shows `persistence.active
= false` alongside `enabled = true`, so the calling model can see that durability is off.
Close the other kaibo, point `--state-db` elsewhere, or pass `--no-persistence` to make it
explicit. Every other open failure stays fatal.

## Media CAS: `[cas]`

The content-addressed store holds generated images, deliberate dossiers, and optional
saved consultation text. It is enabled by default and follows persistence:

| mode | condition | durability |
|---|---|---|
| `disk` | persistence active and a directory resolves | survives restarts |
| `memory` | persistence off/degraded or no directory resolves | this process only; startup warns |
| `off` | `cas.enabled = false` | tools requiring the store are omitted |

```toml
[cas]
enabled = true
# dir = "~/.local/share/kaibo/cas"  # default uses XDG_DATA_HOME when set
# max_bytes = 8589934592           # optional 8 GiB cap; default uncapped
```

`--cas-dir` / `KAIBO_CAS_DIR` select the directory. `--cas-max-bytes` /
`KAIBO_CAS_MAX_BYTES` set the cap. `enabled` is file-only. The directory must stay
outside every allowed project tree. Structural path errors fail startup; permission
or disk-full errors appear on the first write because opening the store writes nothing.

Objects are addressed by SHA-256 and written once, without replacement, deletion,
or eviction. A `<digest>.json` sidecar records the first writer's provenance. Identical
bytes retain that first record; the sidecar is not a per-call audit trail. Retain
digests in your conversation or session. kaibo offers no store listing or cleanup.

`max_bytes` is a soft admission cap. Each write measures the store; a write that would
exceed the cap is refused without eviction. Even duplicate-content writes undergo
admission, so success cannot reveal another project's saved content. An uncapped
store avoids this per-write walk.

On Linux, overlayfs, tmpfs, or ramfs backing produces a warning because the data may
be temporary. `kaibo://config` reports `[cas] backing`. Point `dir` at durable storage
or mount a volume when artifacts must survive the container. The warning does not
prevent intentional temporary use.

### Write and retrieve

| operation | input / output |
|---|---|
| `generate` | stores provider images and returns digests; needs an image slot |
| `deliberate` | stores its dossier; the `dossier` argument reuses it with another synth |
| `save_artifact` | saves consult text when the operator and caller enable it; see below |
| `write_cas` | deposits an image from an allowed `path` or base64 `content`; detects format from bytes |
| `read_cas` | reads one digest, metadata first, with bounded content |
| `kaibo cas write FILE` | deposits an operator-readable image; prints its digest; requires disk mode |
| `kaibo cas read DIGEST` | streams bytes to stdout and metadata to stderr; `--json` combines them |

`kaibo://cas/<digest>` is an artifact identifier, not an MCP resource. Retrieve it with
`read_cas` or `kaibo cas read`. The CLI write command uses the operator's filesystem
access; MCP `write_cas` checks the allowed set. CLI read/write refuse memory mode,
whose contents would disappear between processes.

`read_cas` returns metadata first: digest, URI, MIME type, total bytes, label/provenance,
served range, and the real path in disk mode. Defaults:

| request | text | image | other binary |
|---|---|---|---|
| `length: 0` | metadata only | metadata only | metadata only |
| no `length` | up to 64 KiB from `offset` | whole image up to 5 MiB, otherwise metadata | metadata only |
| explicit `offset` and `length` | text range | base64 range | base64 range |

`length` over 1 MiB is refused. A range splitting a UTF-8 character returns the exact
bytes as base64 with an explanation. Resume from the reported served range.

Only the operator-facing MCP tools and CLI read stored artifacts. The inner model
team cannot read or enumerate the CAS; it spans projects and is never mounted into
kaish. A failed dossier save is logged and the deliberation continues without that
saved record.

## Saving artifacts: `[artifacts]`

`[artifacts] enabled = true` permits the consult driver to save bulk text into the
CAS. It defaults to false. `--allow-save-artifact` (serve only) or
`KAIBO_ARTIFACTS_ENABLED` also enable it.

All three conditions are required:

- Operator enables `[artifacts] enabled`.
- Caller requests `save_artifacts: true`.
- CAS is enabled.

A requested save capability missing either operator setting is refused. Otherwise
`save_artifact` is added to the consult driver's tools, never the explorer's.

| fixed limit | value |
|---|---|
| bytes per artifact | 1 MiB |
| artifacts per call | 8 |
| bytes per call | 8 MiB |
| label | one line, 200 bytes |
| formats | `text`, `jsonl`, `markdown`; unknown hints store as text with a notice |

Oversize content is refused, never truncated. The model receives a digest without
store paths, capacity, or duplicate-content information. It cannot read stored bytes.

Answer and failure footers name artifacts already saved, including MIME type, size,
label, and disk path where available. Session turns persist the footer with the answer.
Cancellation can abort before a footer is returned; allow a saving job to finish to
retain its artifact addresses. Sidecars are first-writer-wins, as described under CAS.

## House rules: `[context]`

```toml
[context]
project_files = ["AGENTS.md", "docs/CONVENTIONS.md"]
user_files = ["~/.config/kaibo/agent-guidance.md"]
```

| list | default | paths | missing file |
|---|---|---|---|
| `project_files` | `["AGENTS.md"]` | root-relative, canonicalized within the project | skipped |
| `user_files` | `[]` | absolute or leading `~` | call fails |

Files are read when a phase builds its preamble. Set `project_files = []` to opt out.
Project symlink or `..` escapes are refused. User files are operator-selected context:
their contents reach the model without making those paths available in kaish.

House rules reach codebase-reading phases: consult, its delegated surveys, standalone
explore, and deliberate's explorer. Toolless oneshot and offline synths get no automatic
project context. CLI file lists replace the lower-layer list; use config or an empty
env value to express an empty list.

## System prompts: `[prompts]`

```toml
[prompts]
explorer = "You are a security reviewer. Ground the report in file:line citations."
```

`[context]` adds guidance; `[prompts]` replaces a phase's role framing. Overrides are
file-only, must contain non-whitespace text, and leave the read-only shell's tool
description intact. House rules still append.

| key | phase |
|---|---|
| `explorer` | every survey, whether its report goes to the caller or a synth |
| `consult` | consult driver |
| `oneshot` | toolless answer |
| `batch` | offline synth |

Write an explorer override suitable for both report audiences. An override replaces
kaibo's tuned framing, including evidence and completion obligations; read the whole
result with `kaibo://prompts`.

### Per-model overrides (the slot `preamble`)

```toml
[casts.custom]
explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "You are a careful reader. Cite exact numbered lines." }
synth = "anthropic/claude-sonnet-4-6"
```

Precedence: `slot.preamble` > `[prompts].<phase>` > built-in. A synth slot's preamble
reaches all its jobs, including consult, oneshot, batch, and deliberate. Use phase keys
when jobs need different framing. A per-call model override drops the configured
slot's preamble and capability/tuning pins. `kaibo://prompts/<cast>` shows the resolved
text. Empty slot preambles are load errors.

## Repo orientation: `[orientation]`

```toml
[orientation]
enabled = true
full_list_max_files = 256
tree_max_depth = 4
```

Each codebase-reading call builds an ignore-aware file map using the same read-only
VFS as the model. Hidden files are included; ignored files are excluded. Up to
`full_list_max_files`, the map lists every file. Larger trees get directory counts to
`tree_max_depth`; if that map also exceeds the line budget, the model gets a note to
use discovery tools. Enumeration failure or an empty tree gives no map.

Both numeric settings must exceed zero; set `enabled = false` to disable orientation.
The map reaches consult and exploring phases, not toolless oneshot or offline synths.

## Path containment

```toml
[server]
root = "~/src/project"
allow_paths = ["~/shared/fixtures"]
```

The **allowed set** bounds model-steered reads. Paths are canonicalized to resolve
symlinks and `..`, then checked against allowed trees. An out-of-bounds path is refused
with the configured trees and ways to change them.

| setting | effect |
|---|---|
| `root` / `--root` | one explicit default project and allowed tree; suppresses inferred cwd |
| `allow_paths` / `--allow-path` | adds trees; does not remove inferred cwd |
| `infer_cwd = true` (default) | adopts launch cwd when no explicit root is set |
| `infer_cwd = false` / `--no-cwd` | disables cwd inference; without explicit root, each call needs `path` |

Check the host's actual launch cwd instead of assuming it is the active project.
`kaibo://config` reports allowed trees, default root, and whether the root was inferred.
`--root` is not repeatable; `--allow-path` is. A nonempty CLI allow-path list replaces
the env/file list. Root and allowed entries must exist and be directories at startup.

File and env path values expand leading `~`, `$VAR`, and `${VAR}`. Unset, empty, or
non-UTF-8 variables fail loading; `$$` represents a literal dollar sign. CLI paths rely
on the caller's shell expansion. `KAIBO_ALLOW_PATHS` is colon-separated.

Name only the projects and scratch directories the user wants models to read.
`allow_paths = ["~/src"]` grants every project beneath that tree; `--allow-path /`
removes the useful read boundary. Scratch access is opt-in, including `/tmp`.
`--state-db` and `--cas-dir` select kaibo-owned storage and must stay outside all
allowed trees.

### Following git worktrees

On by default. When a call's `path` misses the allowed set, kaibo admits it if it is a
linked git worktree of a repo already in the set. A feature branch checked out in a
sibling directory (`git worktree add ../proj-feature …`), including one created
mid-session, is reachable without touching `allow_paths`.

This is narrower than widening to the parent: `--allow-path ~/src` would grant read of
everything under it, while follow admits exactly the worktrees of an already-allowed repo
and nothing else.

**An allowed tree reaches worktrees only when it is a worktree root itself** — it must
hold the `.git` entry, a directory for a main worktree or git's `gitdir:` file for a
linked one. kaibo reads no ancestor of the tree it was given, so a `--root` inside a
larger repository (a package in a monorepo, or any directory under a home with dotfiles
in git) follows nothing rather than reaching that repository's other directories. Name
the repo root as the tree when you want its worktrees.

Worktrees are discovered through git link files, without running git.

**Every link must be named from both ends.** kaibo enumerates the worktrees the allowed
tree's common git dir vouches for, and uses one only when that common dir names the
allowed tree back as a worktree root of its own, and when the worktree in question names
its registration back the way git writes it. So a foreign directory with a forged
`gitdir:` pointer cannot admit itself; a forged `.git` *file* inside the allowed tree
cannot aim the reach elsewhere; and a registration file inside the allowed tree cannot
vouch for a directory that never heard of it. That last one is why the check is per
entry: for a repository cloned to review, `<tree>/.git` and every registration under it
are content kaibo did not author. A vouched worktree that contains the allowed tree is
dropped as well; reach it with `--allow-path`. The check runs only on the
containment-miss path; a normal in-bounds call is untouched.

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

The runtime resource and `kaibo config` show resolved paths, casts, tunables, and key
source metadata. They contain no resolved key values. `api_key_env`, `api_key_file`,
and `api_key_cmd` name the source to diagnose, not its secret.

| section | contents |
|---|---|
| `allowed_paths`, `default_root`, `default_cast` | call scope and defaults |
| `runtime` | advertised tools, missing cast requirements, followed worktrees |
| `tools`, `sandbox`, `kaish.ignore` | configured tool flags and read-only shell limits |
| `defaults`, `backends`, `casts` | effective model configuration and inert-tunable diagnostics |
| `backend_aliases`, `cast_aliases` | alias resolution |
| `persistence`, `cas`, `artifacts` | enabled state, paths, actual modes, and storage warnings |
| `telemetry` | endpoints, signals, capture policy; header names without values |

Followed worktrees are recomputed on each resource read. Configured `enabled` and
runtime `active`/`mode` differ when persistence degrades; read both before relying on
durability.
