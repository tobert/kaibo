# kaibo configuration

Status: **implemented** (`src/config.rs`, tested in `tests/config.rs`). This doc is
the rationale and reference; the code is ground truth.

## Why

Three things drove this:

1. **One `openai` endpoint per process.** `Provider` (`src/credentials.rs`) is an
   enum-as-selector: it fuses *which wire protocol* with *which endpoint, key, and
   models*. So "openai" resolves a single `OPENAI_BASE_URL` + key — you can't have
   hosted GPT **and** local Gemma (or two local backends — llama.cpp via Lemonade
   *and* something else) both selectable in one run. This is the headline need.
2. **Model ids drift and live in code.** `consult.rs::default_models` hardcodes the
   explorer/synth ids per provider; rig's bundled consts rot (we ate a
   `claude-3-5-haiku-latest` 404). They want to live in data.
3. **Three config surfaces don't line up.** CLI flags (`main.rs`), env vars
   (`credentials.rs`), and a pile of hardcoded constants each grew independently.
   A folk who prefers a file has nowhere to put anything.

The fix that unlocks all three is one idea: **split the enum.** A *kind* is the
wire protocol (anthropic / deepseek / gemini / openai — the only thing that selects
a rig client and `thinking_params`). A *profile* is a **named instance** of a kind,
carrying its own base URL, key source, and model ids. `provider` stops meaning
"which of four enums" and starts meaning "which profile by name."

## The model: kinds and profiles

```
ProviderKind  = anthropic | deepseek | gemini | openai   (was: Provider)
Profile       = { name, kind, base_url?, key source, models, per-profile tunables }
```

- **`kind`** is the *only* survivor of the old enum. It drives
  `with_provider_client!` (which rig client) and `thinking_params` (Anthropic
  `thinking` block, Gemini `thinkingConfig`, the rest `None`). It is closed —
  adding a kind means adding a rig client arm in code, not a config line.
- **`base_url`** is meaningful for `kind = "openai"` (the generic
  OpenAI-compatible path). For the keyed kinds rig fixes the endpoint, so a
  `base_url` there is a config error, surfaced loudly — not silently ignored.
- A profile resolves its key from `api_key_env` then `api_key_file` (env wins,
  same precedence as today's `credentials::resolve`). **Secrets never live inline
  in the TOML** — only the *name* of an env var or the *path* to a key file. This
  is deliberate: a config you can commit or paste shouldn't leak a key.
- `key_optional` lets a profile fall back to a placeholder bearer token when no key
  is found (the keyless local-server case — today's `Provider::Openai` behavior).
  Defaults to `true` for `kind = "openai"`, `false` otherwise.
- **`max_tokens` and `thinking_budget` are per-profile overrides** of the
  `[defaults]` values, because they track the *model*, not the server: a hosted GPT
  or Sonnet profile has far more output/reasoning headroom than local Gemma on a
  tight context window. A profile omits them to inherit `[defaults]`. (`max_turns`
  stays a `[defaults]` + per-call concern — it bounds the *loop*, not the model.)
- **`explorer_temperature`, `synth_temperature`, and `top_p` are per-profile
  sampling overrides** of the `[defaults]` values (defaults `0.1` / `0.3` / `0.95`).
  Temperature is per *role*: the explorer gathers exact citations, so it runs cold;
  the synth composes the answer, so it gets a touch more room. Sent where a model
  accepts them — top-level for DeepSeek/OpenAI, under `generationConfig` (camelCase
  `topP`) for Gemini. **Anthropic drops sampling whenever thinking is on** (every
  Anthropic profile, by default): the Messages API 400s on a custom `temperature`
  under thinking, and thinking is the higher-value default, so it wins. Temperature
  must be in `[0.0, 2.0]` and `top_p` in `(0.0, 1.0]`; an out-of-range value is
  rejected at load, not clamped.
- **`explorer_effort` and `synth_effort` are per-role reasoning-depth overrides**
  (default `"high"` both roles) for the models that take an effort param: Anthropic's
  adaptive tier (→ `output_config.effort`) and DeepSeek (→ `reasoning_effort`). A
  passthrough string the provider validates (like a model id), so a new level lands
  without a code change — bump `synth_effort` to `"max"`/`"xhigh"` for heavier synth
  runs. Ignored by models with no effort sink (budget-tier Anthropic, Gemini, OpenAI).
- **`thinking_style` forces the Anthropic thinking shape** (`auto` | `adaptive` |
  `budget`, default `auto`) instead of the built-in classifier. `auto` picks adaptive
  for Opus 4.6+/Sonnet 4.6/Fable 5 and enabled-budget for older models + Haiku 4.5;
  set `adaptive` or `budget` to override when a new or misclassified model ships. A
  no-op for non-Anthropic kinds. An unknown value is a loud load error.
- **`request_timeout_secs` is a per-profile override** (default 900 = 15 min) of the
  per-request LLM deadline: the wall-clock ceiling on a *single* completion call.
  rig's prompt loop is non-streaming and has no native timeout, so a provider that
  connects but never responds would otherwise hang the whole tool call (it once
  waited ~29 min on a wedged local server). It's per-profile because a slow local
  model legitimately wants a longer leash than a hosted API. **Caveat:** because the
  call is non-streaming, this can't tell a *wedged* server from a *slow-but-working*
  one — both look like one long wait — so keep it above your slowest legitimate
  single completion. A value of `0` is rejected at load (it would time out instantly).

### Built-in profiles (the default registry)

The four profiles below ship **in code** and reproduce today's behavior exactly, so
a **missing config file is not an error** — kaibo runs as it does now. The TOML
*merges over* this registry by name: set one field on `openai` to retarget it, or
add a brand-new profile. Built-in aliases are preserved (`claude`→`anthropic`;
`local`/`lemonade`/`gemma`/`gemma4`→`openai`).

| profile (name) | kind | base_url | key env / file | explorer / synth model |
|---|---|---|---|---|
| `anthropic` | anthropic | — | `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt` | `claude-haiku-4-5` / `claude-sonnet-4-6` |
| `deepseek` | deepseek | — | `DEEPSEEK_API_KEY` / `~/.deepseek-key` | `deepseek-v4-flash` / `deepseek-v4-pro` |
| `gemini` | gemini | — | `GEMINI_API_KEY` / `~/.gemini-api-key` | `gemini-flash-lite-latest` / `gemini-3.5-flash` |
| `openai` | openai | `http://localhost:13305/api/v1` | `OPENAI_API_KEY` / `~/.openai-key` (optional) | `Gemma-4-E4B-it-GGUF` / `Gemma-4-26B-A4B-it-GGUF` |

### The multi-openai payoff

Two OpenAI-compatible backends, both live, selected by name:

```toml
[profiles.lemonade]            # llama.cpp via AMD Lemonade, local, keyless
kind = "openai"
base_url = "http://localhost:13305/api/v1"
explorer_model = "Gemma-4-E4B-it-GGUF"
synth_model    = "Gemma-4-26B-A4B-it-GGUF"

[profiles.gpt]                 # hosted OpenAI, keyed
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
key_optional = false
explorer_model = "gpt-5-mini"
synth_model    = "gpt-5"
```

`consult --provider lemonade` and `consult --provider gpt` now both work in one
process. That's the thing the enum couldn't express.

## Three surfaces that line up

Precedence, highest wins:

```
MCP per-call input  >  CLI flag  >  env var  >  config file  >  built-in default
```

Per-call input (the `provider` / `*_model` / `*_max_turns` tool args) is unchanged —
the config supplies the *defaults those override*. The naming rule for everything
else is mechanical:

> config key `foo_bar`  ⇄  env `KAIBO_FOO_BAR`  ⇄  CLI `--foo-bar`

| setting | config key | env var | CLI flag |
|---|---|---|---|
| config file location | — | `KAIBO_CONFIG` | `--config <path>` |
| default root | `server.root` | `KAIBO_ROOT` | `--root` |
| additional allowed trees | `server.allow_paths` *(list)* | `KAIBO_ALLOW_PATHS` *(colon-separated)* | `--allow-path DIR` *(repeatable)* |
| default provider/profile | `server.provider` | `KAIBO_PROVIDER` | `--provider` |
| disable a tool | `server.tools.<t> = false` | `KAIBO_NO_<T>` | `--no-<t>` |
| log filter | `server.log` | `RUST_LOG` *(wins)* / `KAIBO_LOG` | — |
| explorer max turns | `defaults.explorer_max_turns` | `KAIBO_EXPLORER_MAX_TURNS` | *(per-call only)* |
| synth max turns | `defaults.synth_max_turns` | `KAIBO_SYNTH_MAX_TURNS` | *(per-call only)* |
| max output tokens | `defaults.max_tokens` *(per-profile override)* | `KAIBO_MAX_TOKENS` | — |
| thinking budget | `defaults.thinking_budget` *(per-profile override)* | `KAIBO_THINKING_BUDGET` | — |
| explorer temperature | `defaults.explorer_temperature` *(per-profile override)* | `KAIBO_EXPLORER_TEMPERATURE` | — |
| synth temperature | `defaults.synth_temperature` *(per-profile override)* | `KAIBO_SYNTH_TEMPERATURE` | — |
| nucleus top_p | `defaults.top_p` *(per-profile override)* | `KAIBO_TOP_P` | — |
| explorer effort | `defaults.explorer_effort` *(per-profile override)* | `KAIBO_EXPLORER_EFFORT` | — |
| synth effort | `defaults.synth_effort` *(per-profile override)* | `KAIBO_SYNTH_EFFORT` | — |
| thinking style | `defaults.thinking_style` *(per-profile override)* | `KAIBO_THINKING_STYLE` | — |
| LLM request timeout (s) | `defaults.request_timeout_secs` *(per-profile override)* | `KAIBO_REQUEST_TIMEOUT_SECS` | — |
| session cache size | `defaults.session_capacity` *(must be > 0)* | `KAIBO_SESSION_CAPACITY` | — |
| exec timeout (s) | `sandbox.exec_timeout_secs` | `KAIBO_EXEC_TIMEOUT_SECS` | — |
| output cap (bytes) | `sandbox.output_limit_bytes` | `KAIBO_OUTPUT_LIMIT_BYTES` | — |
| disable extra builtins | `sandbox.disable_builtins` *(list; file-only)* | — | — |

**Two deliberate exceptions to the rule:**

- **Provider key vars stay native.** `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`,
  `GEMINI_API_KEY`, `OPENAI_API_KEY` are *not* renamed to `KAIBO_*` — people and
  CI expect those names. A profile points at one via `api_key_env`.
- **`OPENAI_BASE_URL` is kept** as a back-compat override of the built-in `openai`
  profile's `base_url` (it's what's wired today). New profiles use the
  `base_url` config key instead.

`RUST_LOG` is kept (tracing's own convention) and takes precedence; `KAIBO_LOG` and
the `server.log` config key are the lower-precedence ways to set the same filter.

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
- **Malformed TOML, unknown key, or a `base_url` on a keyed kind → hard error at
  startup**, non-zero exit, before `serve()`. We do not silently drop a setting the
  user clearly meant — a typo'd knob that quietly does nothing is the failure mode
  we refuse.
- **An explicit `--config`/`KAIBO_CONFIG` path that doesn't exist → hard error.**
  Only the *default* XDG path is allowed to be absent.
- Keys are still resolved **lazily at call time** (a missing key for an unused
  profile isn't fatal at startup). Startup validation of *which profiles are
  usable* is tracked separately in `docs/issues.md`.

Project-local layering (a repo-root `.kaibo.toml` merged over the user config) is a
plausible later layer — noted, not in this first cut.

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
- `default_provider` — which profile is used when a call omits `provider`
- `tools` — which of the four tools are currently advertised
- `sandbox` — exec timeout, output cap, and any extra disabled builtins
- `profiles` — each provider profile: its kind, models, key source env var name and
  key file path (never the resolved key value), `key_optional`, `base_url` (openai
  kind only), and reasoning-depth fields

**Secret-safety contract:** `kaibo://config` includes key *source metadata* — the
env var name and file path an operator configured — but never the resolved key value.
Keys are resolved lazily at call time from `credentials.rs` and never cached in the
`Config` struct, so the render function has no field that holds a secret. The
`api_key_env` and `api_key_file` names are included deliberately: an operator
debugging a missing-key error needs to see what source the profile is pointing at.

## Code shape (for the implementation follow-up)

- New `src/config.rs`: `Config { server, defaults, sandbox, profiles: BTreeMap<String, Profile> }`,
  `Profile`, and `ProviderKind` (the renamed enum). `serde` + the `toml` crate.
  `Config::load()` = built-in registry → merge file → apply `KAIBO_*` env →
  (CLI applied by `main.rs`). `Config::profile(name) -> Result<&Profile>`.
- `src/credentials.rs`: `Provider` → `ProviderKind`; key resolution stays (`resolve`,
  the env-wins logic) but is driven by a profile's `api_key_env`/`api_key_file`
  rather than the hardcoded per-variant names. `openai_base_url`/`openai_key`
  collapse into per-profile resolution.
- `src/consult.rs`: `with_provider_client!` matches on `profile.kind` and uses
  `profile.base_url` + resolved key, so **any** openai profile constructs a client
  (this is the unlock). `default_models` becomes the built-in registry's seed
  values. `thinking_params` keys off `kind`. `ConsultConfig` reads its tunables
  from `defaults` (still per-call overridable).
- `src/main.rs`: load config, apply CLI overrides on top, pass the resolved
  `Config` into `KaiboHandler`. `--provider` validated against the profile registry
  (was: enum parse).
- `src/server.rs`: `parse_provider` → `resolve profile by name`; per-call model /
  turn overrides layer over the resolved profile + `defaults`.

## TDD seams (tests that can and will fail)

- **Merge precedence**: built-in < file < env < CLI, asserted field-by-field on a
  synthetic config — pure, no real FS/env (mirror `credentials::resolve`'s
  test-pure design).
- **Two openai profiles resolve to two distinct base_urls/keys** from one config —
  the regression that proves the headline bug is fixed.
- **Loud failures**: malformed TOML, `base_url` on a keyed kind, and a missing
  explicit `--config` path each return an error (not a default).
- **Missing file is *not* an error** and yields a registry byte-identical to
  today's `default_models` + built-in credential paths.
- **Native key env vars still win over key files** through a profile's declared
  sources (carry over the existing `credentials.rs` tests).

See `docs/config.example.toml` for the full, commented surface.
