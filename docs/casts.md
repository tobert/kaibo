# Backends, roles, casts — splitting the profile

Status: **implemented** (2026-06-11; `src/config.rs`, `src/consult.rs`,
`src/server.rs`). This doc was the contract for the rewrite and now stands as
its design record, updated where the implementation deliberately diverged.
`docs/config.md` is the configuration reference for the shipped surface.

## Why

`Profile` fuses two selectors — *which wire/endpoint/key* and *which model
serves which role*. That's the same enum-as-selector disease the original
config work cured at the kind/profile level (`docs/config.md` "Why"), one
floor up. Three symptoms surfaced it:

1. **An anthropic profile can never have a voice.** Roles bind to a profile,
   a profile is one `kind`, and Anthropic serves no tts/image model — so
   "consult on Claude, speak via Gemini" had no spelling at all.
2. **A chimera is inexpressible.** The real use case: deepseek explorer,
   claude synth, local image gen, gemini tts — one composed thing selected by
   one name. The fused profile can't say it.
3. **The vocabulary confused its only user.** "Profile" meant *connection* in
   one position and *team* in another. When the words for a design make its
   author lose the thread, the design is wrong, not the author.

The fix is the same as last time: split the fused selector.

## The model

Three concepts, each owning exactly one idea:

- **backend** — a connection: `kind` (the wire protocol — the closed
  `ProviderKind` enum, the one place "provider" still means something),
  `base_url`, key source, `request_timeout`. "How do I reach Gemini."
- **role** — a job a model serves: `explorer`, `synth` (the agent phases),
  `image`, `tts`, later `stt`/`video`/… (the production roles backing kaish
  builtins). Perception is *not* a role — anything the agent must see
  (img2txt, audio-in) is a capability (`ModelCaps`) of an agent slot, because
  the only channel into model context is the rig tool-result envelope. See the
  media-spine entry in `docs/issues.md` for the perception/production split.
  **Status:** the production roles are reserved seams — nothing consumes
  `image`/`tts` yet. `tts` (and `stt`) are parked pending rig provider coverage
  (rig 0.38 has TTS for openai-kind only, no Gemini/Anthropic), to be adopted
  when rig expands rather than hand-rolled. The Gemini-tts slot above is the
  *motivating* example for why casts exist, not working config today.
- **cast** — a named assignment of models to roles, freely spanning backends.
  This is what the `cast` call param selects.

**Selection rule:** calls pick casts; backends are reachable *only through* a
cast's slots. That indirection is the whole cleanup — calls choose a
composition, compositions choose connections.

## Config surface

```toml
# --- backends: connections only. The four built-ins (anthropic, deepseek,
#     gemini, openai) ship in code with today's key sources; a stanza here
#     retargets one or adds a new one. ---

# (No stanza needed for the local llama.cpp default: `gemma` is a built-in
#  alias of the `openai` backend, which already points at localhost:13305.
#  Built-in alias names are reserved — `[backends.gemma]` would be a loud
#  collision error, not a redefinition.)

[backends.sd]                        # local image server, also openai-kind
kind = "openai"
base_url = "http://localhost:7860/v1"
key_optional = true

# --- casts: role → "backend/model". `cast = "chimera"` selects the whole thing. ---

[casts.chimera]
explorer = "deepseek/deepseek-v4-flash"     # cheap fast sweeps
synth    = "claude/claude-sonnet-4-6"       # the voice that answers
image    = "sd/sdxl-turbo"                  # image gen stays local
# tts    = "gemini/gemini-2.5-flash-tts"    # the motivating role — RESERVED, see below

[casts.local-only]                          # privacy posture: nothing leaves the box
explorer = "gemma/Gemma-4-E4B-it-GGUF"
synth    = "gemma/Gemma-4-26B-A4B-it-GGUF"
image    = "sd/sdxl-turbo"

# A slot needing capability pins or tunables takes the table form:
# synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
```

Slot forms: `"backend/model-id"` (the common case; the *first* `/` splits, so
HuggingFace-style ids keep their inner slash) or a table `{ backend, id,
vision?, max_tokens?, thinking_budget?, temperature?, effort?,
thinking_style? }`. A slot ref borrows the backend's *connection only* — it
never follows another cast — so chains and cycles are structurally impossible.
A slot ref may use a backend alias; the slot stores the *canonical* backend
name (resolved at load) so `kaibo://config` renders deterministically.

Rules, in the loud-over-silent house style:

- **Unknown backend in a slot → load error**, naming the known backends.
- **A cast may omit roles.** Built-ins always carry explorer+synth; a user
  cast that omits one is valid config — the tool that needs the missing role
  fails loudly *at call time*, naming the gap ("cast `tts-box` has no synth
  slot"). Absent = capability absent, the same semantics media roles already
  have.
- **`[profiles]` is deleted, not deprecated.** A config that still says
  `[profiles.x]` gets a load error pointing at this doc. Amy is the only
  user; git history is the record.
- **Built-in equivalence:** four built-in backends + four same-named
  single-backend casts. Today's profile aliases — `claude`→`anthropic`,
  `google`→`gemini`, `local`/`lemonade`/`gemma`/`gemma4`→`openai` — register
  at *both* levels (cast aliases so `cast = "claude"` resolves, backend
  aliases so a slot ref `claude/<id>` resolves), and user stanzas can declare
  their own `aliases = [...]` at either level. A missing config file and
  `cast = "anthropic"` reproduce today's behavior byte-for-byte.

## Tunables: what lives where

The split also un-straddles the knobs. **Connection knobs ride the backend**
(key source, `base_url`, `request_timeout` — they describe the wire).
**Model-tracking knobs ride the slot** (`max_tokens`, `thinking_budget`,
temperature, effort, `thinking_style`, `vision` — they describe the model),
falling back to the per-role `[defaults]` exactly as today. A profile-level
`max_tokens` awkwardly shared by two models stops existing.

Consequence in code: `Dialect::from_profile` dissolves. Each arm resolves its
own request shape — `ModelShape::resolve(backend.kind, slot.id, …)` with its
slot's overrides — so a cast whose explorer and synth straddle any capability
line (different kinds, even) is fit per-arm by construction.

## How a call maps

All the chimera-ness happens in one resolution step at the server boundary.
`consult()` never learns about backends — it receives resolved **arms**:

```
server.rs: resolve_cast("chimera")
│
├─ explorer = "deepseek/deepseek-v4-flash"
│    └─ Arm { client: rig(deepseek backend, lazy key), model,
│             params: ModelShape(DeepSeek, model) + explorer effort/temp,
│             caps: vision=false }
├─ synth = "claude/claude-sonnet-4-6"
│    └─ Arm { client: rig(anthropic backend), model, adaptive-thinking params,
│             caps: vision=true → toolset gains view_image (when it lands) }
├─ image = "sd/sdxl-turbo"      ─┐ not consult's business: production slots
└─ tts   = "gemini/…-tts"       ─┘ become kaish builtins at kernel build

consult(question, root, arms, cfg, session)
└─ run_phase(synth_arm): loop over {run_kaish, explore′, view_image…}
     └─ explore′ delegates each sweep to run_phase(explorer_arm)
        — different client, different wire protocol, same loop primitive
```

`cast = "claude"` walks the identical pipeline with boring resolution: the
built-in single-backend cast, both arms on one backend, no media builtins.

- **`explore` / `synthesize`**: one arm each, trivially.
- **`run_kaish`**: shipped *without* a `cast` arg (decided at implementation
  time, overriding the draft here). It has no agent in the loop and no media
  builtins exist yet for a cast's production slots to gate; the arg lands
  with the media spine ("drive the tts with no model in the loop", the
  pal-merge promise).
- **Per-call overrides** (`explorer_model`/`synth_model`, with optional
  `explorer_backend`/`synth_backend` — `model`/`backend` on the single-arm
  tools): the model id rides *verbatim* and the backend is its own explicit
  arg (decided at review time, replacing this draft's qualified `"backend/id"`
  string — in a *call* arg a bare HF org prefix like `google/…` collides with
  the backend aliases, and `contains('/')` would silently retarget the call;
  config slot refs keep the slash form, where a backend is always named). A
  model alone swaps the id within the slot, keeping its backend and dropping
  its caps pins *and per-slot tunables* — they described the configured model;
  the new id classifies fresh. With a backend arg (aliases resolve) the slot is
  replaced wholesale, so it also works on a role the cast doesn't carry — a
  bare id there is a loud error naming the backend arg.
- **The `cast` param** carried `#[serde(alias = "provider")]` for one cycle
  after the rename so a client still sending `provider` selected the named cast
  instead of being *silently ignored* into the default (serde drops unknown
  fields — a textbook silent fallback). That alias is now removed: the inputs
  are `deny_unknown_fields`, so a stale `provider` is a loud invalid-params
  error — the intended end state.
- **Rename map:** `server.provider`→`server.cast`, `KAIBO_PROVIDER`→
  `KAIBO_CAST`, `--provider`→`--cast`, `resolve_profile`→`resolve_cast`;
  `kaibo://config` renders `backends` + `casts` (slots as `"backend/id"` with
  *resolved* caps and only the per-slot tunables actually set) plus a
  `[defaults]` section, so the rendered slots read as deltas against the
  global fallbacks. Every old spelling is a loud tombstone, not a silent
  reinterpretation (see `docs/config.md` "Tombstones").

## The one plumbing fork (decided)

Rig clients are distinct concrete types; today one `with_provider_client!`
monomorphizes a whole consult per kind. Two independent arms would make that
a 4×4 macro product. Decision: **erase the client behind a small internal
`Arm` seam** instead. The calls are network-bound (dispatch is free), the
scripted test client already drives the real loop behind a generic seam, and
16 monomorphizations of a 2k-line module is compile time we'd feel. The
offline mock keys responders by model id, so a mixed cast routing each phase
to its own client is provable with no network.

## What survives untouched

`ModelRole`, `ModelSlot`, `ModelCaps`, `ModelShape`, `run_phase`, sessions,
the sandbox, and path containment. The rewrite is the layer above them — the
2026-06-11 media-spine foundations were built to carry over.

## Build order / TDD seams

1. **Config layer.** Failing-first: built-in equivalence (cast `anthropic`
   resolves today's models, missing file is a non-error), chimera parse
   (string + table slot forms, cross-backend slots, caps classified on the
   *slot's* backend kind), loud errors (unknown backend ref, `[profiles]`
   tombstone, empty model id), alias collisions.
2. **Arms through consult.** Per-phase clients behind the `Arm` seam; the
   offline mock proves a mixed cast drives each phase on its own client and
   that the explore′ delegation crosses backends correctly.
3. **Server surface.** `cast` param (a stale `provider` is a loud unknown-field
   error — the alias was retired one cycle after the rename), `run_kaish` cast
   arg, resource render, `docs/config.md` rewrite, tool descriptions.
