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
selectors — *which connection* and *which model serves which role*. An anthropic
profile could never have a voice (Anthropic serves no tts model); a chimera
(deepseek explorer, claude synth, local image gen, gemini tts) was inexpressible;
and "profile" meant *connection* in one position and *team* in another. So the
profile split too — into **backends** and **casts** — and `[profiles]` is deleted,
not deprecated. The full rationale is `docs/casts.md`.

## The model: backends, roles, casts

```
ProviderKind = anthropic | deepseek | gemini | openai      (the wire protocol)
Backend      = { name, kind, base_url?, key source, request_timeout }
Cast         = { name, role → ModelSlot }                  (freely spans backends)
ModelSlot    = "backend/model-id"  or  { backend, id, pins…, tunables… }
```

Three concepts, each owning exactly one idea:

- **backend** — a *connection*: `kind` (the closed `ProviderKind` enum — the one
  place "provider" still means something; it picks the rig client and the request
  shape), `base_url`, key source, `request_timeout`. "How do I reach Gemini."
- **role** — a *job* a model serves: `explorer`, `synth` (the agent phases),
  `image`, `tts` (the production roles backing media builtins as they land; see
  the media-spine entry in `docs/issues.md`).
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
  local llama.cpp servers, an image server) be live at once, each its own name.
  A *new* openai-kind backend must set it: the `OPENAI_BASE_URL`/local-default
  fallback belongs to the built-in `openai` backend alone, so a forgotten
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
- **`request_timeout_secs`** (default from `[defaults]`, 900 = 15 min) is the
  wall-clock ceiling on a *single* completion call. rig's prompt loop is
  non-streaming and has no native timeout, so a provider that connects but never
  responds would otherwise hang the whole tool call (it once waited ~29 min on a
  wedged local server). It's per-backend because a slow local model legitimately
  wants a longer leash than a hosted API. **Caveat:** a non-streaming call can't
  tell *wedged* from *slow-but-working* — keep it above your slowest legitimate
  single completion. `0` is rejected at load (it would time out every call
  instantly).

### Casts: `[casts.<name>]`

A role table. Each slot is a `"backend/model-id"` string (the common case — the
*first* `/` splits, so HuggingFace-style `org/model` ids keep their inner slash)
or a table when the slot needs pins or tunables:

```toml
[casts.chimera]
explorer = "deepseek/deepseek-v4-flash"     # cheap fast sweeps
synth    = "claude/claude-sonnet-4-6"       # the voice that answers
image    = "sd/sdxl-turbo"                  # image gen stays local
# tts    = "gemini/gemini-2.5-flash-tts"    # RESERVED — parses but unconsumed (see below)

# table form: id + capability pins + per-slot tunables
# synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
```

- **Known roles:** `explorer`, `synth`, `image`, `tts`. A typo'd role (or a typo'd
  per-slot knob) is a loud load error, not a silent no-op. A cast may omit roles —
  built-ins always carry explorer+synth; a user cast that omits one is valid
  config, and the tool that needs the missing role fails loudly *at call time*,
  naming the gap ("cast `tts-box` has no synth slot"). Absent = capability absent.
  (Nothing consumes `image`/`tts` yet; they land with the production builtins.
  `tts` — and a future `stt` — is parked pending rig provider coverage: rig 0.38
  drives TTS only for openai-kind backends, not Gemini/Anthropic. Kept as the
  adoption seam; the shipped `config.example.toml` deliberately omits it rather
  than advertise a capability that doesn't exist. See `docs/issues.md`.)
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
`temperature`, `effort`, `thinking_style`, the `vision` pin — they describe the
model), each falling back to its per-role `[defaults]` value when omitted
(`explorer` slots inherit the `explorer_*` defaults; `synth` and the media roles
inherit the `synth_*` side). A profile-level `max_tokens` awkwardly shared by two
models no longer exists.

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
  (→ `output_config.effort`), DeepSeek (→ `reasoning_effort`), and the Gemini
  3-line (→ `thinkingLevel`: the values align, `"high"`/`"low"`). A passthrough
  string the provider validates (like a model id), so a new level lands without a
  code change — set a synth slot's `effort = "max"`/`"xhigh"` for heavier runs.
  Ignored by models with no effort sink (budget-tier Anthropic/Gemini, OpenAI).
- **`thinking_style`** (`auto` | `adaptive` | `budget`, default `auto`) forces the
  Anthropic thinking shape instead of the built-in classifier. `auto` picks
  adaptive for Opus 4.6+/Sonnet 4.6/Fable 5 and enabled-budget for older models +
  Haiku 4.5; set `adaptive` or `budget` when a new or misclassified model ships. A
  no-op for non-Anthropic kinds. An unknown value is a loud load error.
- **`request_timeout_secs`** seeds every backend (see above).
- **`explorer_max_turns` / `synth_max_turns`** (100 / 200) stay in `[defaults]`
  only (plus per-call): they bound the *loop*, not the model.
- **`session_capacity`** (128, must be > 0): max multi-turn consult sessions held
  in memory (LRU, capacity-evicted, no TTL).

### Built-in registry (the defaults)

Four backends and four same-named single-backend casts ship **in code** and
reproduce kaibo's historical behavior exactly, so a **missing config file is not
an error**. The TOML *merges over* this registry by name: set one field on a
built-in to retarget it, or add brand-new backends and casts. The built-in alias
names register at **both** levels — as cast aliases (so `cast = "claude"`
resolves) and backend aliases (so a slot ref `claude/<id>` resolves) — and are
reserved: naming a new backend or cast after one is a loud collision error.

| backend | kind | base_url | key env / file | aliases |
|---|---|---|---|---|
| `anthropic` | anthropic | — | `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt` | `claude` |
| `deepseek` | deepseek | — | `DEEPSEEK_API_KEY` / `~/.deepseek-key` | — |
| `gemini` | gemini | — | `GEMINI_API_KEY` / `~/.gemini-api-key` | `google` |
| `openai` | openai | `http://localhost:13305/api/v1` | `OPENAI_API_KEY` / `~/.openai-key` *(optional)* | `local`, `lemonade`, `gemma`, `gemma4` |

| cast | explorer | synth |
|---|---|---|
| `anthropic` | `anthropic/claude-haiku-4-5` | `anthropic/claude-sonnet-4-6` |
| `deepseek` | `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-pro` |
| `gemini` | `gemini/gemini-flash-lite-latest` | `gemini/gemini-3.5-flash` |
| `openai` | `openai/Gemma-4-E4B-it-GGUF` | `openai/Gemma-4-26B-A4B-it-GGUF` |

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
`synth_backend` on consult, `backend` on explore/synthesize) retargets the slot
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
| session cache size | `defaults.session_capacity` *(must be > 0)* | `KAIBO_SESSION_CAPACITY` | — |
| exec timeout (s) | `sandbox.exec_timeout_secs` | `KAIBO_EXEC_TIMEOUT_SECS` | — |
| output cap (bytes) | `sandbox.output_limit_bytes` | `KAIBO_OUTPUT_LIMIT_BYTES` | — |
| scratch cap (bytes) | `sandbox.scratch_limit_bytes` *(must be > 0; default 64 MB)* | `KAIBO_SCRATCH_LIMIT_BYTES` | — |
| disable extra builtins | `sandbox.disable_builtins` *(list; file-only)* | — | — |
| telemetry on/off | `telemetry.enabled` *(default false)* | `KAIBO_TELEMETRY_ENABLED` | — |
| OTLP traces endpoint | `telemetry.endpoint` | `KAIBO_TELEMETRY_ENDPOINT` | — |
| export timeout (s) | `telemetry.timeout_secs` *(must be > 0)* | `KAIBO_TELEMETRY_TIMEOUT_SECS` | — |
| trace service name | `telemetry.service_name` | `KAIBO_TELEMETRY_SERVICE_NAME` | — |
| export headers | `telemetry.headers` *(map; file-only — values are secrets)* | — | — |

**Two deliberate exceptions to the rule:**

- **Provider key vars stay native.** `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`,
  `GEMINI_API_KEY`, `OPENAI_API_KEY` are *not* renamed to `KAIBO_*` — people and
  CI expect those names. A backend points at one via `api_key_env`.
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
`gen_ai.request.model` and every `gen_ai.usage.*` token count) → a `tool` span per
`run_kaish` / delegated `explore′` sweep. kaibo only adds the named parent spans
(`consult` / `explore` / `synthesize` / `run_kaish`) that root each trace; the
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

## Path containment

**Always on.** Every tool call's `path` argument (or the server `--root` when `path`
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

**Widening the boundary:**

```toml
# config.toml
[server]
allow_paths = ["/home/atobey/shared-libs", "/data/fixtures"]
```

```sh
# env — colon-separated like PATH
KAIBO_ALLOW_PATHS=/home/atobey/shared-libs:/data/fixtures kaibo

# CLI — repeatable
kaibo --allow-path /home/atobey/shared-libs --allow-path /data/fixtures
```

A non-empty CLI `--allow-path` set replaces the env/file layer entirely (same
precedence rule as `--root`). To lift all limits: `--allow-path /`.

**Containment ≠ defaulting.** With no `--root`, an omitted `path` still errors
("no `path` provided and the server has no default `--root`") — the launch cwd
bounds what you *may ask about*, it never becomes what you *asked about*. The error
is `invalid_params`, surfaced where the caller can read it.

**Resolution.** `resolve_root` (`src/server.rs`) returns the *canonicalized* path,
so the kaish VFS mount target is always resolved. A nonexistent or non-directory
entry in `--root` / `--allow-path` is a loud construction error at startup.

## kaibo://config

An MCP resource at the URI `kaibo://config` (`application/toml`) exposes the server's
resolved runtime state. Reading it before making calls tells the calling model (or an
operator) the full picture:

- `allowed_paths` — the canonicalized trees a per-call path must be at-or-under
- `default_root` — the `--root` value, if set
- `default_cast` — which cast is used when a call omits `cast`
- `tools` — which tools are currently advertised (`consult`, `explore`,
  `synthesize`, `run_kaish`, `generate_image`)
- `sandbox` — exec timeout, output cap, scratch (`/` MemoryFs) cap, and any extra disabled builtins
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
