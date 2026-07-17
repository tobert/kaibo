# kaibo configuration

Status: **implemented** (`src/config.rs`, tested in `tests/config.rs`). This doc is
the rationale and reference; the code is ground truth. The design record for the
backends/casts split is `docs/casts.md`.

## Why

The config surface was carved in two passes, each splitting a fused selector.

**Pass one** (kinds and profiles) was driven by three things:

1. **One `openai` endpoint per process.** The original `Provider` enum fused *which
   wire protocol* with *which endpoint, key, and models* — so "openai" resolved a
   single `OPENAI_BASE_URL` + key, and hosted GPT **and** local Gemma couldn't both
   be selectable in one run.
2. **Model ids drift and live in code.** Hardcoded explorer/synth ids rot (we ate a
   `claude-3-5-haiku-latest` 404). They want to live in data.
3. **Three config surfaces don't line up.** CLI flags, env vars, and a pile of
   hardcoded constants each grew independently. A folk who prefers a file had
   nowhere to put anything.

The fix was to split the enum: a *kind* is the wire protocol, a *profile* a named
instance of one.

**Pass two** found the same disease one floor up. A profile still fused two
selectors — *which connection* and *which model serves which role*. A profile is
one `kind`, so a team was locked to one family: a chimera (a cheap local deepseek
explorer feeding a claude synth) was inexpressible, and "profile" meant
*connection* in one position and *team* in another. So the profile split too —
into **backends** and **casts** — and `[profiles]` is deleted, not deprecated. The
full rationale is `docs/casts.md`.

## The model: backends, roles, casts

```
ProviderKind = anthropic | deepseek | gemini | openrouter | openai   (the wire protocol)
Backend      = { name, kind, base_url?, key source, request_timeout }
Cast         = { name, role → ModelSlot }                  (freely spans backends)
ModelSlot    = "backend/model-id"  or  { backend, id, pins…, tunables… }
```

Three concepts, each owning exactly one idea:

- **backend** — a *connection*: `kind` (the closed `ProviderKind` enum — the one
  place "provider" still means something; it picks the rig client and the request
  shape), `base_url`, key source, `request_timeout`. "How do I reach Gemini."
- **role** — a *job* a model serves: `explorer` and `synth`, the two agent
  phases (there are no output/production roles — kaibo reasons over code and
  renders nothing). A slot that reads images carries a `vision` pin; perception
  is a slot capability, not a role.
- **cast** — a *composition*: a named assignment of models to roles. This is what
  the `cast` call param selects.

**Selection rule:** calls pick casts; backends are reachable *only through* a
cast's slots. Calls choose a composition, compositions choose connections. A slot
ref borrows the backend's connection only — it never follows another cast — so
chains and cycles are structurally impossible.

### Backends: `[backends.<name>]`

Connection knobs only — models never live here:

- **`kind`** selects the rig client and request shaping. It is closed — adding a
  kind means adding a client arm in code, not a config line. A *new* backend must
  declare its kind (it seeds that kind's default key sources); re-listing an
  existing backend with a *different* kind is a loud error — you don't change a
  connection's protocol by re-declaring it.
- **`base_url`** is meaningful for `kind = "openai"` (the generic OpenAI-compatible
  path) — this is what lets any number of openai-kind backends (hosted GPT, two
  local llama.cpp servers, an Ollama box) be live at once, each its own name.
  A *new* openai-kind backend must set it: the `OPENAI_BASE_URL`/local-default
  fallback belongs to the built-in `openai-local` backend alone, so a forgotten
  `base_url` is a load error, not a silent dial of the wrong server. For the
  keyed kinds rig fixes the endpoint, so a `base_url` there is a config error,
  surfaced loudly — not silently ignored.
- A backend resolves its key from `api_key_env` then `api_key_file` (env wins).
  **Secrets never live inline in the TOML** — only the *name* of an env var or the
  *path* to a key file. A config you can commit or paste shouldn't leak a key.
- `key_optional = true` falls back to a placeholder bearer token when no key is
  found (the keyless local-server case). Defaults to `true` for `kind = "openai"`,
  `false` otherwise. A key file that's *present but broken* (empty, unreadable) is
  a loud error even for a keyless backend — there-but-wrong is a mistake, not
  "keyless".
- **`kind = "openrouter"`** is a keyed gateway, not a wire protocol of its own: one
  `OPENROUTER_API_KEY` reaches every upstream model family through a fixed endpoint
  (`base_url` there is the same loud load error as on the other keyed kinds).
  Reasoning is **on by default on every slot** — OpenRouter's unified
  `{"reasoning":{"effort":…}}` request field, which the gateway translates into
  each upstream provider's native knob and drops silently where the pinned model
  has none, so emitting it unconditionally never breaks a non-reasoning model. The
  per-role `effort` (see [defaults] below) passes through verbatim, reaching
  OpenRouter-only rungs (`xhigh`, `max`) deeper than the other kinds expose. No
  `batch` lane. One slug routes across competing upstream *hosts* whose data
  policies differ, so every request pins **no-collection routing by default**
  (`provider.data_collection = "deny"` — kaibo's prompts carry your source, and
  shipping it to a host that retains or trains on prompts must be a choice, never
  a default). A model whose only hosts collect (most `:free` variants) fails
  loudly instead of leaking quietly. `data_collection = "allow"` on the backend
  is the explicit opt-in — kaibo then emits no restriction and your OpenRouter
  account settings govern. The knob exists only on this kind (a load error
  elsewhere), and `kaibo://config` renders the active policy per openrouter
  backend so the posture is always visible.
- **`request_timeout_secs`** (default from `[defaults]`, 900 = 15 min) is the
  wall-clock ceiling on a *single* completion call. rig's prompt loop is
  non-streaming and has no native timeout, so a provider that connects but never
  responds would otherwise hang the whole tool call (it once waited ~29 min on a
  wedged local server). It's per-backend because a slow local model legitimately
  wants a longer leash than a hosted API. **Caveat:** a non-streaming call can't
  tell *wedged* from *slow-but-working* — keep it above your slowest legitimate
  single completion. `0` is rejected at load (it would time out every call
  instantly).

  **Failure policy (no retry, by design).** kaibo does not retry a failed provider
  call — there is no backoff and no `max_retries` knob. A 429/503/529 overload, a
  connection reset, a partial stream, or a wedged backend that hits `request_timeout`
  all fail the single completion, and `consult`/`oneshot` surface that as a **clean
  tool-result error** (`is_error`) naming the cast and the underlying detail. The
  rationale: a consult is an *optional* augmentation, so the calling agent should read
  the failure and proceed without the second opinion (or call again) rather than have
  its own tool call fail at the protocol layer. The message is classified so the agent
  can drive the right next step: a *transient* condition (overload / rate-limit /
  timeout / reset) invites a manual retry, a non-transient one (auth / bad request) does
  not, and a kaibo-side failure is named as such. (Classification is a heuristic on the
  provider's error *vocabulary*, not the HTTP status — rig surfaces the response body,
  not the code.) Retrying is the caller's decision; for a reliably-slow backend, raise
  its `request_timeout_secs` rather than expecting kaibo to paper over it. Automatic retry/backoff belongs in the shared HTTP layer (rig already
  ships an `ExponentialBackoff`, wired only into its streaming path today) — landing it
  for the non-streaming completion path is tracked as an upstream contribution in
  `docs/issues.md`, not hand-rolled here.

### Casts: `[casts.<name>]`

A role table. Each slot is a `"backend/model-id"` string (the common case — the
*first* `/` splits, so HuggingFace-style `org/model` ids keep their inner slash)
or a table when the slot needs pins or tunables:

```toml
[casts.chimera]
explorer = "deepseek/deepseek-v4-flash"     # cheap fast sweeps — local/cheap family
synth    = "claude/claude-sonnet-4-6"       # the voice that answers — hosted family

# table form: id + capability pins + per-slot tunables
# synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
# explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "..." }  # per-model prompt
```

- **Known roles:** `explorer`, `synth`. A typo'd role (or a typo'd per-slot knob)
  is a loud load error, not a silent no-op. A cast may omit roles — the four
  interactive built-ins carry explorer+synth, the batch built-ins carry synth
  only; a user cast that omits a role is valid config, and the tool that needs the
  missing role fails loudly *at call time*, naming the gap ("cast `lite` has no
  synth slot"). Absent = capability absent. (There are no output/production roles:
  kaibo reasons over a codebase and renders nothing, so image/tts-style "make an
  artifact" roles don't exist. Perception — image input, audio-in later — is a
  slot's `vision`/`ModelCaps` capability, below, not a role.)
- **An unknown backend in a slot is a load error** naming the known backends; an
  empty model id is rejected at load (it would surface as a baffling provider 404
  mid-call otherwise).
- **`vision` pins the slot's vision capability** (accepts image parts in model
  context), overriding the built-in classifier. The classifier runs against the
  *slot's backend kind*: Anthropic and Gemini completion models are multimodal-in;
  DeepSeek chat/reasoner are text-only; a generic `openai` endpoint is vision-off
  until its config says otherwise (kaibo can't know what's behind an arbitrary id —
  opt in, don't guess). Resolved caps (not raw config) are what `kaibo://config`
  reports, and they will gate toolset assembly when vision-in lands.
- Backends *and* casts both take a file-level `aliases = [...]` list. An alias
  that collides with a real name at its level, or that two names both claim, is a
  loud load error.

### Tunables: what lives where

The split un-straddles the knobs. **Connection knobs ride the backend** (key
source, `base_url`, `request_timeout_secs` — they describe the wire).
**Model-tracking knobs ride the slot** (`max_tokens`, `thinking_budget`,
`temperature`, `effort`, `thinking_style`, the `vision` pin, and `preamble` — they
describe the model), each falling back to its per-role `[defaults]` value when
omitted (`explorer` slots inherit the `explorer_*` defaults; `synth` inherits the
`synth_*` side). A profile-level `max_tokens` awkwardly shared by
two models no longer exists. The `preamble` knob is the per-model system prompt —
its own fallback chain (it has no `[defaults]` entry) is documented under
[System prompts](#system-prompts-prompts) below.

The `[defaults]` knobs themselves:

- **`max_tokens` / `thinking_budget`** (16384 / 8192): output headroom and
  reasoning budget. Reasoning eats the *completion* budget, so `max_tokens` must
  sit well above `thinking_budget` — for slots on Anthropic- or Gemini-kind
  backends an inverted pair is rejected at load on the slot's *resolved* values
  (Anthropic would 400 on it mid-call).
- **`explorer_temperature` / `synth_temperature` / `top_p`** (0.1 / 0.3 / 0.95):
  sampling per role — the explorer gathers exact citations, so it runs cold; the
  synth composes the answer, so it gets a touch more room. Sent where a model
  accepts them (top-level for DeepSeek/OpenAI, under `generationConfig` for
  Gemini). **Anthropic drops sampling whenever thinking is on** (every Anthropic
  slot, by default): the Messages API 400s on a custom `temperature` under
  thinking, and thinking is the higher-value default, so it wins. Temperature must
  be in `[0.0, 2.0]` and `top_p` in `(0.0, 1.0]` — at the `[defaults]` level *and*
  per slot, an out-of-range value is rejected at load, not clamped.
- **`explorer_effort` / `synth_effort`** (`"high"` both): reasoning depth for the
  models that take an effort param — Anthropic's adaptive tier
  (→ `output_config.effort`), DeepSeek (→ `reasoning_effort`), the Gemini
  3-line (→ `thinkingLevel`: the values align, `"high"`/`"low"`), and OpenRouter
  (→ its unified `{"reasoning":{"effort":…}}`, forwarded verbatim to whatever the
  pinned model actually supports). A passthrough string the provider validates
  (like a model id), so a new level lands without a code change — set a synth
  slot's `effort = "max"`/`"xhigh"` for heavier runs; OpenRouter is where those
  deeper rungs are actually reachable today. Ignored by models with no effort
  sink (budget-tier Anthropic/Gemini, OpenAI).
- **`thinking_style`** (`auto` | `adaptive` | `budget`, default `auto`) forces the
  Anthropic thinking shape instead of the built-in classifier. `auto` picks
  adaptive for Opus 4.6+/Sonnet 4.6/Fable 5 and enabled-budget for older models +
  Haiku 4.5; set `adaptive` or `budget` when a new or misclassified model ships. A
  no-op for non-Anthropic kinds. An unknown value is a loud load error.
- **`request_timeout_secs`** seeds every backend (see above).
- **`call_deadline_secs`** (default 3600 = 1 h, must be > 0) is the whole-*call*
  wall-clock ceiling on an interactive `consult`/`explore`/`oneshot` — the backstop
  for when the per-request `request_timeout` doesn't fire (a stalled response body, a
  pooled keep-alive to a wedged backend). Past it the call aborts with a clean
  tool-result error instead of hanging your session. Keep it **above the largest
  `request_timeout` a call can reach** so it never cuts a legitimately slow single
  completion — operators running a >30-min local model should raise it. It bounds the
  interactive loop tools — `consult`/`explore`/`oneshot` and async `consult_submit`.
  Two in-process paths sit outside it *by nature*: `deliberate`'s direct lane is one
  long completion bounded instead by its synth backend's `request_timeout` (+ a small
  margin) — so a slow local `deliberate` gets its full patience *without* forcing this
  interactive ceiling up to hours; and the **batch** lane holds no in-process wait at
  all (the work runs on the provider's queue, collected by polling `job_get`).
- **`explorer_max_turns` / `synth_max_turns`** (100 / 200) stay in `[defaults]`
  only (plus per-call): they bound the *loop*, not the model.
- **`session_capacity`** (128, must be > 0): max multi-turn consult sessions held
  in memory (LRU, capacity-evicted, no TTL).
- **`job_capacity`** (64, must be > 0): max async-`consult` jobs (`consult_submit`)
  held in memory — running plus finished-but-uncollected (LRU, capacity-evicted, no
  TTL; evicting a still-running job aborts it). Its own knob, smaller than
  `session_capacity` because a held job result is heavier than a session's Q&A pair.
- **`inline_attach_budget`** (262144 = 256 KiB; `0` is legal): cumulative byte budget
  for inlining `consult` text attachments into the driver prompt (caller order,
  greedy). A text attachment past the remaining budget is *demoted* — named in the
  prompt with a read-it-WHOLE directive instead of its bytes — loudly, never dropped.
  `0` inlines nothing (every text attachment becomes a directive): the escape hatch
  for a small-context cast, e.g. a 4K-ctx local model that chokes on inlined bytes a
  hosted model shrugs at. Inlined bytes ride every turn of the driver loop, so this
  bounds resident prompt cost, not just one request. The tool-less tools
  (`oneshot`/`batch_submit`) are unaffected — with no shell to fall back on, they
  keep their own hard per-file/per-call caps.

### Built-in registry (the defaults)

Five backends and five same-named single-backend casts ship **in code** and
reproduce kaibo's historical behavior exactly, so a **missing config file is not
an error**. The `openrouter` built-in pins the drift-resistant `~author/family-latest`
catalog aliases (explorer `~google/gemini-flash-latest`, synth
`~anthropic/claude-sonnet-latest`) rather than a dated slug, and opts both slots into
`vision` — the classifier defaults an `openrouter` slot to vision-off, since the
gateway fronts blind and sighted models alike, but both default ids are
multimodal-in per OpenRouter's own catalog. Two extra built-in casts ship for the
offline batch lane —
`gemini-batch` (synth Gemini Pro) and `anthropic-batch` (synth Claude Opus). Lane is
a **per-slot** property, not a cast-level one: each carries a synth slot whose
`lane = "batch"`, which staffs `batch_submit` *only*: the interactive tools
(`consult`/`oneshot`) refuse a cast whose synth is on an offline lane, and
`batch_submit` refuses a cast whose synth isn't specifically `lane = "batch"`, so a
big offline-tuned model is never run interactively by accident (and vice versa). A
`batch`-lane synth must sit on a batch-capable backend (Anthropic or Gemini) —
declaring `lane = "batch"` on a slot elsewhere is a loud load error, and so is a
lane on an *explorer* slot (the explorer always runs interactively). Because lane
lives on the slot, a cast MAY pair an interactive explorer with an offline synth —
the built-in batch casts stay synth-only by choice (batch is toolless, so an
explorer would be dead weight), not by a rule. `batch = true` at the cast level is
backward-compat sugar: it sets the synth slot's `lane = "batch"`, nothing more —
there's exactly one internal representation of lane (the slot field). The TOML
*merges over* this registry by name: set one field on a built-in to retarget it (a
slot's `lane` is sticky across a bare re-declaration of its model — retuning
`gemini-batch`'s id leaves it batch), or add brand-new backends and casts. The
built-in alias names register at **both** levels — as cast aliases (so
`cast = "claude"` resolves) and backend aliases (so a slot ref `claude/<id>`
resolves) — and are reserved: naming a new backend or cast after one is a loud
collision error.

A second offline lane, `lane = "direct"`, is also validated and rendered: one long
completion kaibo drives itself against a big *local* model — no async provider API,
just a slower plain call. It's reserved for offline deliberation over a model too
slow for a live tool loop; no tool routes to a `direct` synth yet, so declaring one
today is forward-looking, not yet callable from `consult`/`oneshot`/`batch_submit`.

| backend | kind | base_url | key env / file | aliases |
|---|---|---|---|---|
| `anthropic` | anthropic | — | `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt` | `claude` |
| `deepseek` | deepseek | — | `DEEPSEEK_API_KEY` / `~/.deepseek-key` | — |
| `gemini` | gemini | — | `GEMINI_API_KEY` / `~/.gemini-api-key` | `google` |
| `openrouter` | openrouter | — *(fixed)* | `OPENROUTER_API_KEY` / `~/.openrouter-key` | — |
| `openai-local` | openai | `http://localhost:13305/api/v1` | `OPENAI_API_KEY` / `~/.openai-key` *(optional)* | `local`, `lemonade`, `gemma`, `gemma4` |

| cast | explorer | synth | synth lane |
|---|---|---|---|
| `anthropic` | `anthropic/claude-haiku-4-5` | `anthropic/claude-sonnet-4-6` | |
| `deepseek` | `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-pro` | |
| `gemini` | `gemini/gemini-flash-lite-latest` | `gemini/gemini-3.5-flash` | |
| `openrouter` | `openrouter/~google/gemini-flash-latest` | `openrouter/~anthropic/claude-sonnet-latest` | |
| `openai-local` | `openai-local/Gemma-4-E4B-it-GGUF` | `openai-local/Gemma-4-26B-A4B-it-GGUF` | |
| `gemini-batch` | — | `gemini/gemini-pro-latest` | `batch` |
| `anthropic-batch` | — | `anthropic/claude-opus-4-8` | `batch` |

### The chimera payoff

The thing the fused profile couldn't say: each role on the backend that serves it
best, selected as one name. Two extra openai-kind connections, one composed cast:

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
synth    = "gpt/gpt-5"                  # the answer gets the big model
```

`cast = "mixed"`, `cast = "gpt"` (if you define it), and the built-in
`cast = "anthropic"` all walk the same resolution: each slot becomes an *arm* —
client on the slot's backend, request shape fit to the slot's model and tunables —
so a cast whose explorer and synth straddle any capability line (different kinds,
even) is fit per-arm by construction.

## Three surfaces that line up

Precedence, highest wins:

```
MCP per-call input  >  CLI flag  >  env var  >  config file  >  built-in default
```

Per-call input is the `cast` / `*_model` / `*_backend` / `*_max_turns` tool args —
the config supplies the *defaults those override*. A per-call model override is a
model id sent *verbatim* (an id containing `/`, HuggingFace-style, is still one
id — it is never parsed for a backend, so an org prefix matching a backend alias
can't silently retarget the call); it swaps the id within the configured slot,
dropping its pins and per-slot tunables — they described the configured model;
the new id classifies fresh. The matching backend arg (`explorer_backend` /
`synth_backend` on consult, `backend` on oneshot) retargets the slot
to another backend (a call-time chimera: aliases resolve, and it works even on a
role the cast doesn't carry). The naming rule for everything else is mechanical:

> config key `foo_bar`  ⇄  env `KAIBO_FOO_BAR`  ⇄  CLI `--foo-bar`

| setting | config key | env var | CLI flag |
|---|---|---|---|
| config file location | — | `KAIBO_CONFIG` | `--config <path>` |
| default root | `server.root` | `KAIBO_ROOT` | `--root` |
| additional allowed trees | `server.allow_paths` *(list)* | `KAIBO_ALLOW_PATHS` *(colon-separated)* | `--allow-path DIR` *(repeatable)* |
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

**Two deliberate exceptions to the rule:**

- **Provider key vars stay native.** `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`,
  `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY` are *not* renamed to
  `KAIBO_*` — people and CI expect those names. A backend points at one via
  `api_key_env`.
- **`OPENAI_BASE_URL` is kept** as a back-compat override for any openai-kind
  backend that doesn't set an explicit `base_url` (it's what's wired today). New
  backends use the `base_url` config key instead.

`RUST_LOG` is kept (tracing's own convention) and takes precedence; `KAIBO_LOG` and
the `server.log` config key are the lower-precedence ways to set the same filter.

### Tombstones (the `provider` spellings)

The rename map ships as loud errors, never silent reinterpretation:

| old spelling | what happens now |
|---|---|
| `[profiles.<name>]` | load error pointing at `[backends]` + `[casts]` and `docs/casts.md` |
| `server.provider` | unknown-field load error (`deny_unknown_fields`) |
| `KAIBO_PROVIDER` | load error naming `KAIBO_CAST` and `docs/casts.md` |
| `--provider` | rejected by clap (unknown flag) |
| call arg `provider` | unknown-field error (`deny_unknown_fields`) — the alias is gone |

The call-arg `provider` alias was the one survivor for a single cycle after the
rename: serde drops unknown fields, so without it a client still sending
`provider` would have been *silently ignored* into the default cast — a textbook
silent fallback. That cycle is over; the alias is removed and a stale `provider`
is now a loud invalid-params error like every other tombstone above.

## File location & loading

XDG, with explicit overrides:

```
$KAIBO_CONFIG                           # explicit path wins
--config <path>                         # ... or this
$XDG_CONFIG_HOME/kaibo/config.toml      # default
~/.config/kaibo/config.toml             # when XDG_CONFIG_HOME unset
```

Loading rules, in the spirit of "crashing beats silent corruption":

- **Missing file → built-in defaults, no error.** kaibo works out of the box.
- **Malformed TOML, an unknown key (including a typo'd role or per-slot knob), a
  `base_url` on a keyed kind, an unknown backend in a slot, an empty model id, an
  out-of-range sampling value, an inverted `thinking_budget`/`max_tokens` pair on
  a thinking-kind slot, an alias collision, or an unresolvable `server.cast` →
  hard error at startup**, non-zero exit, before `serve()`. We do not silently
  drop a setting the user clearly meant — a typo'd knob that quietly does nothing
  is the failure mode we refuse.
- **An explicit `--config`/`KAIBO_CONFIG` path that doesn't exist → hard error.**
  Only the *default* XDG path is allowed to be absent.
- Keys are still resolved **lazily at call time** (a missing key for an unused
  backend isn't fatal at startup). Startup validation of *which backends are
  usable* is tracked separately in `docs/issues.md`.

Project-local layering (a repo-root `.kaibo.toml` merged over the user config) is a
plausible later layer — noted, not in this cut.

## Telemetry (OpenTelemetry traces)

**Off by default, and that default is load-bearing.** kaibo reads a private
codebase, and the spans `rig-core` emits carry prompts, completions, and source
snippets. A default run must ship *nothing* off-box, so `[telemetry]` is opt-in:

```toml
[telemetry]
enabled      = true                                # default false
endpoint     = "http://localhost:4318/v1/traces"   # OTLP/HTTP traces receiver
timeout_secs = 10                                  # per-export deadline; must be > 0
service_name = "kaibo"                             # service.name on the trace Resource
headers = { authorization = "Bearer <token>" }     # file-only; values are secrets
```

What you get is the GenAI trace tree rig already produces — a tool call →
`run_phase` per phase → `invoke_agent` → a `chat` span per model turn (with
`gen_ai.request.model` and every `gen_ai.usage.*` token count). On top of that,
kaibo adds the named parent spans (`consult` / `oneshot` /
`run_kaish`) that root each trace, **and a `tool` span per tool invocation**
(`tool_span.rs`) carrying `gen_ai.tool.name` and an ok/err `outcome` — so you can
query *which* tool the model actually called (`run_kaish`, `view_image`, the nested
`explore′`), not just that a turn happened. rig's own per-tool instrumentation
isn't reliably queryable across backends, so this is kaibo's, on kaibo's tools. The
exporter ships the rest. Transport is OTLP/HTTP + protobuf (the `/v1/traces` path),
reusing kaibo's `reqwest` — no gRPC, no second HTTP stack.

**The boundary this draws.** Enabling opens an **outbound** OTLP connection to
`endpoint`. That's allowed under kaibo's stdio-only invariant — kaibo can read a
filesystem, so it must never *bind* a socket, but reaching *out* to a collector is
not binding. Keep `endpoint` local (the default `localhost:4318`) unless you mean
to send traces — with full content — to a remote. Header **values** are secrets and
never appear in the `kaibo://config` render (only the header *names* do, like an
API-key env-var name). Logs continue to ride the `tracing` → stderr + MCP
`notifications/message` path regardless; telemetry adds the *traces* signal only.

## Persistence: `[persistence]`

**On by default.** kaibo keeps a small state db so a `consult` session thread and the
provider batch handles you launch **survive a server restart** and are shared across
front doors (start a session over MCP, continue it from the CLI). It lives at a fixed
XDG state path, never a path a model controls:

```toml
[persistence]
enabled = true                                  # default true
path    = "$XDG_STATE_HOME/kaibo/state.db"      # default; else ~/.local/state/kaibo/state.db
```

CLI/env: `--no-persistence` / `KAIBO_NO_PERSISTENCE` disable it (in-memory, like before);
`--state-db <FILE>` / `KAIBO_STATE_DB` move the db. `path` is `$VAR`/`~`-expanded like
`root`/`allow_paths`.

**What persists:** the lean `(question, answer)` turns of each session (capacity-evicted,
no TTL — same as the in-memory store), and the `{backend, provider-id, label}` of each
batch you submit (so `job_list` can re-surface a handle after a restart). **What never
persists:** background *consult/deliberate* job handles (`job-N` — in-memory, session-only
by design) and exploration reports (ephemeral by design — they'd be stale bloat). The db
is a convenience layer, **never a source of truth**: safe to delete, and its content is
model output, never anything the calling model steers onto disk.

**Read-only toward your project is unchanged.** The store is handler-side, at the XDG
path — kaish's read-only sandbox never sees it, kaibo still writes nothing into any
project, and `open` **refuses a state-db path that resolves inside an allowed tree** (so
it can't be pointed into a repo). See `docs/kaibo-persistence-and-cli.md` and the
"Read-only is the product" invariant.

**Loud on failure, never a silent fallback.** If the store can't open (a bad path, a db
inside a project, a locked file on a single-process platform, a network mount — turso's
multiprocess mode is 64-bit-Unix + local-fs only), kaibo **fails to start** with an error
naming the escape hatch, rather than quietly dropping to memory and losing your sessions
on the next restart. On Windows the store is single-process: a second kaibo opening the
same db fails loudly (close the other, or `--no-persistence`).

## House rules: `[context]`

kaibo's models are *for other agents*, so it helps when they inherit the calling
agent's conventions. `[context]` names files whose contents are spliced into each
consultation tool's preamble (the system prompt) as standing guidance — an
`AGENTS.md`, a shared `~/.claude/CLAUDE.md`, whatever you call yours. **Vendor-
neutral:** no filename is hardcoded in the product; the only default is the
emerging cross-tool `AGENTS.md` convention, and that's just a config default you
can change or turn off.

```toml
[context]
# Root-relative, read IF PRESENT (absent is normal). Default: ["AGENTS.md"].
# An explicit [] opts out of even that.
project_files = ["AGENTS.md", "docs/CONVENTIONS.md"]

# Absolute/tilde paths, read UNCONDITIONALLY (a missing one is a startup-visible
# error — you declared it, so kaibo won't silently drop it). Default: none.
user_files = ["~/.claude/CLAUDE.md"]
```

Two lists, two deliberately different failure semantics:

- **`project_files`** are root-relative and **read-if-present**: a repo with no
  `AGENTS.md` is the normal case, not an error. Each is joined to the resolved
  project root and canonicalize-checked to stay *within* it — a configured `../`
  or an out-of-tree symlink is refused, so the same containment that bounds the
  read-only shell bounds what gets injected.
- **`user_files`** are **read-required**: you named the file on purpose, so a
  missing one is a loud error rather than a silent skip that ships an answer
  missing the guidance you were counting on.

**The trust boundary** (why `user_files` may sit outside the allowed set): these
files are read in trusted server-side Rust at the tool handler — the same trust
level as `config.toml` itself — and only their *contents* reach the model, never
the path. The read-only kaish shell still cannot reach `~/.claude`; the model's
read scope is *not* widened. That's the distinction from `[server] allow_paths`
below: `allow_paths` widens what the *model* can explore, `[context]` injects
fixed operator text the model never navigates to.

Injected into the codebase-reading phases — the `consult` driver *and* its nested
`explore′` sweep — so the cheap explorer orients on the same `AGENTS.md`/user guidance
while it searches, not just at answer time (the block names where things live and
what matters). Precedence is the usual per-call > CLI > env > file > built-in: a
CLI `--project-context-file` replaces lower layers additively (the CLI can't
express "empty"; opt out with an empty `[context] project_files = []` or
`KAIBO_PROJECT_FILES=`).

## System prompts: `[prompts]`

Where `[context]` *adds* project guidance, `[prompts]` *replaces* the built-in
role framing — the system prompt each phase runs under. One override per phase:

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

**Full replace, by decision.** An override *is* the role framing, verbatim — kaibo
does not re-wrap it. That's safe because the kaish operating contract (how to drive
the read-only shell, the exit-code meanings, the `cat -n`/`grep -rn` idioms) rides the
`run_kaish` *tool description* independently, so the model keeps the shell contract
even when you rewrite the prose. What an override *does* drop is the tuned role
framing kaibo ships — the explorer's "report, don't conclude", the synth's "trust a
grounded citation, reach for more", the positive-framing discipline weak/local
models lean on. That's yours to own when you override.

**Orthogonal to `[context]`.** House rules still append on top of an override:
`[prompts]` sets the *role*, `[context]` adds the *project's* conventions, and both
land in the final system prompt. Layering order is `override-or-built-in` →
`+ house rules`.

**File-only, and operator-only.** Multiline prose has no clean env/CLI form (the
same call `telemetry.headers` makes), so overrides live only in `config.toml`. They
are *not* a per-call tool argument — a calling agent can't inject a system prompt;
only the operator who owns the config can. An empty or whitespace-only override is
a **loud load error** (a blank system prompt is never intended) — remove the key to
fall back to the built-in.

### Per-model overrides (the slot `preamble`)

`[prompts]` is keyed by *phase* (the job). A prompt can also be keyed by *model*,
because the same phase may run different models — a local Gemma explorer wants
different framing than a Claude Haiku one (kaibo's request shaping is already
model-aware; the prose is too). The per-model knob is `preamble` on the cast's
**slot**, beside `effort`/`thinking_style`:

```toml
[casts.local]
explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "You are a careful reader; quote exact lines." }
synth    = "anthropic/claude-sonnet-4-6"   # no per-model prompt; uses [prompts] or built-in
```

**Precedence, per phase:** `slot.preamble` → `[prompts].<phase>` → built-in. The
slot (most specific — *this* model in *this* cast) wins, the same way `effort`
overrides the `[defaults]` effort. Set neither and the built-in runs.

**One model, two synth jobs.** The synth slot's model plays *both* the `consult`
driver and the toolless `oneshot`, so its `preamble` feeds both — but each phase
resolves under its own key, so they stay **independently overridable**: a copy today
(both the synth slot's voice), free to diverge by setting `[prompts].consult` and
`[prompts].oneshot` separately. `slot.preamble` = "this model's voice"; the phase
keys = "this job's framing." The explorer has one job, so no ambiguity. A per-call
model override (a bare slot) carries no `preamble` — overriding the model doesn't
drag the configured slot's framing along. Same loud-on-empty rule as `[prompts]`.

`batch_submit` runs the synth model too, but deliberately does **not** inherit the
synth slot's `preamble`: its lane has a distinct behavioral contract (one offline
response, no follow-up, spend on depth) that a slot preamble written for interactive
synth would silently replace. Tune batch through `[prompts].batch` (or accept the
built-in `batch_preamble`); the slot preamble stays scoped to the interactive phases.

## Repo orientation: `[orientation]`

A static, computed-once **file map** injected into the exploring preamble, so a
model starts *knowing* the project's files instead of spending its first turns on
`glob`/`ls`/`find` to discover the layout (the structure-first lesson from
Agentless/Aider, made free — no model in the loop).

```toml
[orientation]
enabled = true               # default; set false to turn the map off
full_list_max_files = 256    # ≤ this → inject the full file list; above → directory map
tree_max_depth = 4           # how deep the fallback directory map descends
```

How it works: the server runs the kernel's **own** `glob -a --json '**/*'` server-side
per `explore`/`consult` call — the *same* ignore-aware enumeration the model's shell
would get (same VFS, same ignore rules), so the map can't disagree with what the
explorer's own `glob`/`grep` sees. `-a` includes hidden config (`.github/`, `.cargo/`);
the ignore filter still drops `.git`/`target`.

Size-gated, with a graceful descent — orientation is an *enhancement* (the model always
has `glob`/`grep`/`explore′`), so its absence is never fatal and the call is never refused
for being large:
- **≤ `full_list_max_files`** → the complete file list is spliced in.
- **above it** → a **directory map**: the same files folded into a depth-limited tree of
  `dir/  N files` lines, descending `tree_max_depth` levels (deeper files stay counted at
  the deepest shown directory). The names are traded for structure; the model recovers them
  with `glob 'DIR/**/*'` / `grep -rn`.
- **above it *and* the directory map would itself exceed `full_list_max_files` lines** (a very
  large or very wide repo) → a short **discover-as-you-go note** naming the discovery tools.
  Skipped maps are logged (`tracing::warn`), not silent.

`full_list_max_files = 0` is a load error (it would refuse every repo — disable instead), as is
`tree_max_depth = 0` (it would render an empty map).

It rides the **exploring** phases only — the `consult` driver and its nested
`explore′` sweep. The toolless `oneshot` reads no project, so it gets no map. Like
`[context]`, the block re-sends each turn, which the size gate
keeps bounded. Whether it actually erases the discovery turns is measurable via the
per-tool `tool` spans (see Telemetry).

## Path containment

**Always on.** Every tool call's `path` argument (or the default root when `path`
is omitted) is resolved — `std::fs::canonicalize` expands symlinks and collapses `..`
— and then checked against the **allowed set**. A path that doesn't fall at-or-under
one of the allowed trees is `invalid_params`, naming the allowed trees and the three
knobs that widen them.

**The allowed set** is constructed at startup from the canonicalized `--root` plus
every canonicalized `--allow-path`. When both are absent the allowed set defaults to
the canonicalized launch cwd. MCP clients start stdio servers with cwd = workspace,
so the zero-config case scopes itself to the project naturally, without any operator
action. The default isn't silent: the resolved allowed set is reported in a startup
log line and in the `## Scope` section of the server's MCP `instructions` (visible in
every `initialize` response), and at `kaibo://config`.

**The default root** is what a call resolves to when it omits `path`: an explicit
`--root`, or — when none is set — the launch cwd, *inferred* whenever it falls inside
the allowed set (it always does in the zero-config case, since the cwd is the whole
allowed set). So the common single-workspace case needs no `--root`: kaibo already
knows the workspace from its cwd and uses it for both bounding and defaulting. The
inferred case is labelled as such in the `## Scope` handshake and at `kaibo://config`
(`default_root_inferred`). The one gap: an `--allow-path` that *excludes* the cwd
leaves no default root — kaibo never defaults to a path its own containment check
would then reject, so an omitted `path` there stays an error.

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

A non-empty CLI `--allow-path` set replaces the env/file layer entirely (same
precedence rule as `--root`). To lift all limits: `--allow-path /`.

**Set it once.** Putting your whole workspace tree in `allow_paths`
(`["~/src"]`) means every project under it is in-bounds, and because the client's
cwd/workspace lands inside that tree, kaibo infers it as the [default root](#path-containment)
automatically — so you configure access once and never pass `path` per call.

**Path expansion.** In `root` / `allow_paths`, the file and env layers expand a leading
`~` to `$HOME` *and* `$VAR` / `${VAR}` from the environment; the CLI relies on your shell's
own expansion instead. Paths with no `~`/`$` are taken as written. A variable that is unset,
**set but empty**, or non-UTF-8 is a loud load error rather than a silent gap that would
misplace the boundary — an empty value matters because `$EMPTY/scratch` would collapse to
`/scratch` and `$EMPTY/` to `/` (the whole filesystem). Write `$$` for a literal `$`; a
stray `$` that begins no reference is itself an error, so a typo can't slip through as a
literal segment. (A directory literally named `$foo` is written `$$foo`.)

**Reading a scratch / temp space.** kaibo reads only what's in the allowed set and never
writes anywhere — so to let it read artifacts a workflow drops in a temp dir (a diff, a
generated file, a log), add that dir to `allow_paths`. Write it portably with the env var
rather than a host-specific literal, so it resolves on whatever machine kaibo runs on:

```toml
[server]
allow_paths = ["~/src", "$TMPDIR", "$XDG_RUNTIME_DIR/kaibo"]
```

`$TMPDIR` (POSIX) and `$XDG_RUNTIME_DIR` (XDG) land on the per-user scratch dir on macOS
and sandboxed Linux respectively, where a bare `/tmp` would be wrong. This is an opt-in:
widening to a shared, world-writable space like `/tmp` is a real (read-only) boundary
move, so kaibo never adds it for you.

**When defaulting does *not* happen.** If `--allow-path` is set to a tree that does
not contain the launch cwd and no `--root` is given, there is no default root: the
cwd is outside the boundary, so adopting it would point the default at a path
containment rejects. An omitted `path` then errors ("no `path` provided and the
server has no default root …") — `invalid_params`, surfaced where the caller can read
it. Pass an explicit `--root` (inside an allowed tree) to restore a default.

**Resolution.** `resolve_root` (`src/server.rs`) returns the *canonicalized* path,
so the kaish VFS mount target is always resolved. A nonexistent or non-directory
entry in `--root` / `--allow-path` is a loud construction error at startup.

**Following git worktrees (on by default).** When a call's `path` misses the allowed
set, kaibo doesn't reject it outright if it's a *linked git worktree of a repo
already in the set* — it admits it. So a feature branch you check out in a sibling
directory (`git worktree add ../proj-feature …`), even one created mid-session, is
reachable without touching `allow_paths`. This is *narrower* than widening to the
parent (`--allow-path ~/src` would grant read of everything under it); follow admits
exactly the worktrees of an already-allowed repo and nothing else.

kaibo resolves this by **reading git's own link files** — a worktree's `.git` file,
the repo's `.git/worktrees/<name>/{gitdir,commondir}` — never by running `git` (the
binary isn't in the build; see [the sandbox probe runbook](sandbox-probes.md)). Trust flows only
outward from the allowed repo: kaibo enumerates the worktrees the *allowed* repo's
common git dir vouches for and admits a candidate only if it falls inside one. It
never consults the candidate's own `.git`, so a foreign directory with a forged
`gitdir:` pointer can't admit itself. The check runs only on the (rare)
containment-miss path — the normal in-bounds call is untouched.

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

An MCP resource at the URI `kaibo://config` (`application/toml`) exposes the server's
resolved runtime state. Reading it before making calls tells the calling model (or an
operator) the full picture:

- `allowed_paths` — the canonicalized trees a per-call path must be at-or-under
- `default_root` — the `--root` value, if set
- `default_cast` — which cast is used when a call omits `cast`
- `runtime` — state *computed at read time*, kept distinct from the configured
  knobs above so a reader can tell "what kaibo discovered" from "what the operator
  set". Currently `follow_worktrees` (the knob's effective value) and
  `followed_worktrees` (the git worktrees admitted beyond `allowed_paths` right
  now; recomputed each read, so a worktree added mid-session appears without a
  reconnect)
- `tools` — which tools are currently advertised (`consult`, `oneshot`,
  `run_kaish`)
- `sandbox` — exec timeout, output cap, scratch (`/` MemoryFs) cap, and any extra disabled builtins
- `kaish.ignore` — the resolved ignore policy the file-walking builtins honor:
  `files`, `defaults`, `auto_gitignore`, `global_gitignore`, `scope`
- `defaults` — the global tunables every slot falls back to (rendered so the
  per-slot values below read as deltas against it)
- `backends` — each connection: its kind, the *resolved* `base_url` (openai kind),
  key source env var name and key file path (never the resolved key value),
  `key_optional`, and `request_timeout_secs`
- `backend_aliases` / `cast_aliases` — alias → canonical name, built-in and
  file-declared both: every name a `cast` param, slot ref, or per-call backend
  override will resolve
- `casts` — each composition's slots as `model = "backend/id"` (canonical backend
  name) with the *resolved* `vision` capability (slot pin applied, else the
  classifier) and only the per-slot tunables actually set

**Secret-safety contract:** `kaibo://config` includes key *source metadata* — the
env var name and file path an operator configured — but never the resolved key value.
Keys are resolved lazily at call time and never cached in the `Config` struct, so the
render function has no field that holds a secret. The render destructures `Backend`,
`ModelSlot`, `Defaults`, `ToolGating`, and `SandboxConfig` exhaustively, so a new
field is a compile error at the render site — an explicit render-or-skip (and
secret-review) decision, not a silent omission. The
`api_key_env` and `api_key_file` names are included deliberately: an operator
debugging a missing-key error needs to see what source the backend is pointing at.

See `docs/config.example.toml` for the full, commented surface, and `docs/casts.md`
for the design record of the backends/casts split (including how a cast resolves
into per-phase arms).
