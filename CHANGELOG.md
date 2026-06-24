# Changelog

All notable, user-facing changes to kaibo are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kaibo aims for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`0.2.0` is the first tracked release — the point kaibo adopts a pull-request
workflow and a maintained changelog. It captures the feature set as kaibo goes
public rather than reconstructing the 0.1 development line; that history lives in
the git log. Each later release appends a new section at the top.

## [0.2.0] — unreleased

### Added

- **`consult`** — the headline tool: ask a model *outside your own family* about a
  codebase and get a grounded, cited answer. A capable model reads precise spans
  directly and delegates broad sweeps to a cheap explorer sub-agent, then synthesizes
  — so your context receives the answer, not the investigation transcript. Pick which
  family answers with `cast`. Optionally seed it with `context` (a change summary or
  pasted source), trusted as starting evidence while it investigates for more. The
  answer carries a provenance footer naming the cast and the models that produced it.
  Args: `question`, `context`, `path`, `cast`, `session_id`, `include_report`, and
  per-call `explorer_model` / `synth_model` (+ `_backend`) overrides.
- **`consult_submit`** — the *async sibling* of `consult` (as batch is to `oneshot`):
  start a consultation in the background and get back a handle (`job-N`) instead of
  holding your turn open while a deep investigation runs. Same investigation, same args
  as `consult`. Built for running several consults at once — a cross-model study submits
  one per `cast` and collects them all — or for not blocking on a long answer: submit, go
  do other work, collect later. Jobs are in-memory and live only for the server session
  (no restart survival), evicted by capacity like sessions. Replaces the pattern of
  spawning a throwaway sub-agent just to hold a blocking `consult` open. On completion a
  job emits a soft notification on the MCP logging channel (a clue for a client watching
  the log stream) — advisory only, since no MCP primitive wakes the calling agent;
  collecting by handle stays the contract.
- **`oneshot`** — a thin, direct second opinion from a model outside your family:
  prompt in, answer out, no codebase access and no tools, exactly one upstream
  request. The counterpart to `consult` for when you already own the context (you've
  pasted what's needed, or the question is general). Pick the model with `cast`; the
  answer carries the same provenance footer. Takes the same **`attach`** as
  `batch_submit` — name workspace files ("review README.md", or `git diff > x.diff`
  then `attach: ["x.diff"]`) and kaibo inlines them (text as text, images as native
  image parts on a vision-capable model) so their bytes never pass through your context.
  So "call Opus once with these files, no tools, no waiting" is a single call.
- **`run_kaish`** — drive the read-only kaish shell yourself, no model in the loop:
  exit code + stdout + stderr.
- **`generate_image`** — kaibo's first *capability* (an artifact handed back to the
  caller, not reasoning run into kaibo's own models): prompt → image, returned inline
  as MCP image content. OpenAI-compatible image backends only (hosted
  `gpt-image` / DALL·E, or a local Stable-Diffusion server). Its `cast` parameter
  advertises the casts that actually carry a usable image slot as a schema enum, so a
  host agent picks one off the schema — as discoverable as the consultation tools.
- **Batch (`batch_submit`)** — the *offline, async sibling* of `oneshot`: submit a list
  of tool-less prompts, get a handle, then collect it with the shared `get`/`cancel`/
  `list` verbs (see below) — read every answer when the provider's batch lane finishes,
  no call held open per answer. Built for fanning many prompts (or one hard question you'll wait on) at a
  top-tier model: it maxes the knobs (forces high thinking effort + a generous token
  budget) regardless of how the cast was tuned for interactive use, and a per-call
  `model`/`backend` override lets you batch a Pro/Opus tier a cast otherwise synths
  cheaper. Each prompt is self-contained — no codebase access, no tools. kaibo keeps
  no state: the handle is the whole address, so poll/cancel survive a restart, and a
  failed item is surfaced per-item rather than dropped. Runs on **Anthropic and Gemini**
  backends (OpenAI batch is a tracked follow-on); a cast whose synth has no batch lane is
  refused with a clear message naming the ones that do. For Gemini there's a ready-made
  `gemini-batch` cast that synths Gemini **Pro** — the tier you reach for offline, where
  its latency is free. Gated by `--no-batch` (one flag over every verb). Batch carries its
  own system preamble fit to the offline lane — one complete, self-contained response with
  no follow-up, told to spend on depth — overridable via `[prompts].batch` like the other
  phases. While a batch runs, `get` reminds you to go do other work and check back
  rather than wait on it. Lost a handle? `list` re-discovers the batches a backend
  still holds (newest first, each with its handle, status, and progress), so a batch is
  never orphaned — defaulting across every batch-capable backend, or scoped to one with
  `backend`. **`attach`** lets you name workspace files to inline as shared context for
  every prompt — "review README.md" with `attach: ["README.md"]`, or `git diff > x.diff`
  and `attach: ["x.diff"]` — so the file's bytes never pass through your own context.
  Text files splice in as text; images (png/jpeg/gif/webp) ride as native image parts
  (with a vision-capable synth model). Paths obey the same workspace boundary as
  everything else (worktrees included); a file outside it, a directory, an oversized
  file, or a binary that isn't a known image is refused with a clear error.
- **`get` / `cancel` / `list`** — one shared surface to collect, stop, and survey
  *both* kinds of async work, told apart by the handle: a batch handle is
  `backend/provider-id`, a consult job is `job-N`. `get <handle>` returns a progress/
  status line while the work runs and the full result when it lands; `cancel <handle>`
  stops it; `list` shows everything in flight — your in-memory consult jobs plus the
  batches each backend still holds — each with a ready-to-use handle. One mental model
  for everything you submit. The verbs stay available as long as either `consult` or
  batch is enabled (gated off only when both are). `list` trims its batch section to the
  **last 24 hours** by default — a provider keeps months of finished batches and dumping
  them all just burns tokens, while anything older is done and still collectible by its
  handle; it reports how many it hid and takes `all: true` for the full history (true
  orphan recovery). Consult jobs are always shown in full.
- **`view_image`** — vision-capable consultation phases can read an image *file* from
  the workspace into model context (screenshots, diagrams, assets already in the tree).
- **Multi-provider model teams.** Anthropic, DeepSeek, and Gemini natively, plus a
  generic `openai` kind for any OpenAI-compatible endpoint (hosted GPT, local
  llama.cpp / Ollama / Gemma). Configured as **backends** (connections), **casts**
  (named teams), and **roles** (explorer / synth / image); a cast can mix families
  across roles — a cheap local explorer with a hosted synth. Built-in casts ship so
  kaibo runs with zero config; `config.toml` merges over them. Precedence:
  per-call > CLI > env > file > built-in, and a missing config file is not an error.
- **Guided setup.** A built-in `configure` MCP prompt walks your host agent through
  writing `config.toml`, alongside `kaibo://config` (resolved runtime state) and
  `kaibo://config/example` (annotated template) resources. Secrets are referenced by
  env-var name or key-file path, never inlined. `kaibo://config` flags any per-slot
  tunable the slot's resolved model shape will never send (an `inert_tunables` list —
  e.g. a `thinking_budget` on an effort-only model, an `effort` on a budget-only one),
  so a no-op knob is visible to the operator instead of rendering as if effective.
- **Zero-config workspace root.** When no `--root` is set, kaibo adopts its launch
  cwd as the inferred default root (it already scoped containment to that cwd, and
  MCP clients start stdio servers with cwd = workspace), so a call may omit `path`
  and still land on the project. The scope handshake and `kaibo://config` tag the
  root as inferred. An `--allow-path` that excludes the cwd leaves no default root —
  kaibo never defaults to a path its own containment check would reject.
- **`~` *and* `$VAR` / `${VAR}` expand in every config path — `[server] root`,
  `allow_paths`, `[context] user_files`, and a backend's `api_key_file`** (config-file
  and `KAIBO_*` env layers). One uniform rule: you never have to remember "env vars work
  here but not there." `user_files = ["$XDG_CONFIG_HOME/notes.md"]` and
  `api_key_file = "$XDG_CONFIG_HOME/keys/anthropic"` now resolve per-environment instead
  of failing on a literal `$` (those two were previously tilde-only). Set
  `allow_paths = ["~/src"]` once and every project
  under it is in-bounds — with cwd inferred as the default root, you stop thinking about
  `path` entirely. (Previously a literal `~` was taken verbatim and failed
  canonicalization at startup.) Environment variables make a scratch space portable:
  `allow_paths = ["$TMPDIR"]` or `["$XDG_RUNTIME_DIR/kaibo"]` lets kaibo read artifacts
  a workflow drops in a temp dir without hardcoding a host-specific `/tmp`. A variable
  that is unset, **set but empty**, or non-UTF-8 is a loud load error, never a silent gap
  that would misplace the read boundary (an empty `$EMPTY/` would otherwise collapse to
  `/`); write `$$` for a literal `$`. The `configure` prompt now walks you through this
  opt-in.
- **Follow git worktrees automatically.** A `path` in a linked git worktree of an
  already-allowed repo is now reachable without an `--allow-path` — so a sibling
  branch you check out next to the project (even one you spin up mid-session) just
  works. kaibo resolves this by reading git's own link files, never by running git
  (the binary still isn't in the build). Trust flows only outward from the allowed
  repo: a forged `.git` in a foreign directory can't admit itself. The
  `kaibo://config` `[runtime]` section shows which worktrees are currently followed.
  Turn it off with `--no-follow-worktrees`, `KAIBO_NO_FOLLOW_WORKTREES`, or
  `[server] follow_worktrees = false` to keep the boundary strictly static.
- **Per-tool gating.** Each tool has a `--no-<tool>` flag (all on by default); an
  all-off server is refused at startup.
- **Operator ignore files** via a `[kaish.ignore]` config stanza.
- **Thinking on by default,** with model-aware request shaping (per-provider thinking
  config, per-role reasoning effort, generous completion-token headroom).
- **Multi-turn sessions** via `session_id`, and optional OTLP/HTTP trace export
  (`[telemetry]`, off by default).
- **A failed provider doesn't fail your turn.** When a model or its provider misbehaves
  (a 429/529 overload, a connection reset, a wedged backend that hits the
  `request_timeout`), `consult`/`oneshot` return a *clean tool-result error* naming the
  cast and the underlying detail — so the calling agent reads "the consult failed, here's
  why" and proceeds without the second opinion, instead of its own tool call failing at
  the protocol layer. The message is tailored to the failure: a *transient* condition
  (overload / rate-limit / timeout) invites a manual retry the agent can drive, a
  non-transient one (auth / bad request) doesn't, and a kaibo-side error is named as such
  rather than blamed on the provider. kaibo does not retry automatically (a consult is
  optional augmentation); the policy is documented in the README FAQ and `docs/config.md`.
- **Single self-contained binary** per platform; Linux builds are fully static
  (musl). TLS is rustls + ring — no OpenSSL, no aws-lc, no C toolchain.

### Security

- **Read-only is structural, not best-effort.** kaibo compiles in only kaish's
  `localfs` axis — `subprocess` / `git` / `host` / `os-integration` are off, so
  `exec` / `spawn` / `git` / `ps` don't exist in the binary — and mounts the project
  read-only, with an in-memory scratch filesystem for everything else. Reads are
  scope-bounded to `--root` / `--allow-path` (launch cwd by default), enforced after
  symlink and `..` canonicalization.
- **Bounded resource use.** Each kaish script is capped (30 s wall-clock, 8 KB
  output, 64 MB scratch — over-cap fails loudly, never a silent drop), and the model
  loops stop at turn limits, so a runaway consultation can't melt the machine or the
  budget. All configurable.
- **Attachments are read through the read-only VFS.** A named attachment's bytes are
  read *through* the same read-only kaish mount the shell uses (rooted at the file's
  containing allowed tree), not a separate `std::fs` read after the containment check.
  The VFS refuses to follow a symlink out of the allowed tree at read time, so a path
  swapped for an out-of-tree symlink *after* the check is rejected structurally rather
  than by racing a re-check — the boundary holds regardless of timing.
- **Attachment wrappers can't be confused by their own contents.** Neither an attached
  file's *body* nor its *name* can forge the `<file>` wrapper boundary anymore. A body
  holding a `<file>`-tag lookalike — a `</file>` close, a stray opening `<file …>`, or a
  whitespace/case variant — is escaped, and the caller's path (a legal filename can hold
  `"`, `>`, or newlines) is attribute-escaped, so a maliciously-named file can't inject a
  second wrapper. The line between an attachment and the prompt stays unambiguous across
  `oneshot` and batch.

[0.2.0]: https://github.com/tobert/kaibo/releases/tag/v0.2.0
