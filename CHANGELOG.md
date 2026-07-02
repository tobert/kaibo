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
  Args: `question`, `context`, `path`, `cast`, `session_id`, `attach`, `include_report`,
  and per-call `explorer_model` / `synth_model` (+ `_backend`) overrides. **`attach`**
  names workspace files (under the project root) to put in front of the investigation —
  unlike the tool-less tools' attach, kaibo does *not* inline them; it names them in the
  prompt and the consult model opens each itself when it's ready, in full, building its own
  narrative: a text file with the shell (`cat -n`), an **image** with its `view_image` tool.
  An attached image therefore needs a vision-capable cast — kaibo refuses one to a
  vision-blind synth up front (the same honest refusal `oneshot`/`batch` give) rather than
  name a file the model could never open. The files just have to live under the root the
  consult reads (a worktree counts).
- **`consult_submit`** — the *async sibling* of `consult` (as batch is to `oneshot`):
  start a consultation in the background and get back a handle (`job-N`) instead of
  holding your turn open while a deep investigation runs. Same investigation, same args
  as `consult`. Built for running several consults at once — a cross-model study submits
  one per `cast` and collects them all — or for not blocking on a long answer: submit, go
  do other work, collect later. Jobs are in-memory and live only for the server session
  (no restart survival), evicted by capacity (LRU) via its own `[defaults] job_capacity` /
  `KAIBO_JOB_CAPACITY` knob (default 64). Replaces the pattern of
  spawning a throwaway sub-agent just to hold a blocking `consult` open. On completion a
  job emits a soft notification on the MCP logging channel (a clue for a client watching
  the log stream) — advisory only, since no MCP primitive wakes the calling agent;
  collecting by handle stays the contract.
- **`explore`** — the evidence-gathering half of `consult`, exposed on its own: a fast,
  cheap explorer model sweeps the project READ-ONLY and hands back the *cited report
  itself* — a summary of findings, the relevant `file:line` locations, and the trail it
  followed — with no synthesis on top. Reach for it to map unfamiliar code, or to build a
  grounded survey you'll reason over yourself (or feed to another model), when you want the
  map rather than the conclusion. It reads the repo itself like `consult`, so it takes the
  same `path` / `cast` / `explorer_model` (+ `explorer_backend`) / `explorer_max_turns`
  arguments; being single-phase, it has no synth args, `attach`, `context`, or `session_id`.
  Because it runs *only* the explorer, its `cast` accepts **any cast with an explorer** —
  not just interactive ones, but `deliberate`/`direct` casts too: point it at one to run
  that team's (often smarter) explorer standalone, handy for sizing up an explorer or for a
  stronger sweep than your own fast one. The report carries the same provenance footer,
  naming the cast and the explorer that surveyed. Gated independently by `--no-explore`. For
  a synthesized answer, use `consult`.
- **`deliberate`** — a top model's deepest reasoning on your codebase without holding a
  session open: `explore → offline synth`. A fast model first investigates READ-ONLY and
  builds a cited dossier (you wait for this — the same live sweep `explore` runs), then a
  heavyweight synth reasons over that evidence *offline*. The synth's lane (a per-slot
  property of its cast) picks the mechanism: **`batch`** — a frontier model on the
  provider's batch lane (max thinking, half price), returning a durable `backend/provider-id`
  handle the moment the dossier is submitted (collect it any time, even after a restart);
  or **`direct`** — one long completion on a big *local* model, returning a session-scoped
  `job-N` (`job_wait`/`job_get` it; a restart loses it). Needs a cast pairing an interactive
  explorer with an offline synth (the example config's `fable`, `gemini-deliberate`, or
  `local-direct`) — `deliberate`'s `cast` enum lists the usable ones and `kaibo://config`
  shows each cast's lane. Reads the repo itself, so it takes `path` / `cast` /
  `explorer_model` / `synth_model` (+ `_backend`s); the offline synth is a single turn, so
  no `attach` / `context` / `session_id`. Gated independently by `--no-deliberate`. This is
  the tool that finally routes the `direct` lane the per-slot lane reshape introduced. For
  an answer this turn, use `consult`.
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
- **Batch (`batch_submit`)** — the *offline, async sibling* of `oneshot`: submit a list
  of tool-less prompts, get a handle, then collect it with the shared `job_get`/`job_cancel`/
  `job_list` verbs (see below) — read every answer when the provider's batch lane finishes,
  no call held open per answer. Built for fanning many prompts (or one hard question you'll wait on) at a
  top-tier model: it maxes the knobs (forces high thinking effort + a generous token
  budget) regardless of how the cast was tuned for interactive use, and a per-call
  `model`/`backend` override lets you batch a Pro/Opus tier a cast otherwise synths
  cheaper. Each prompt is self-contained — no codebase access, no tools. kaibo keeps
  no state: the handle is the whole address, so poll/cancel survive a restart, and a
  failed item is surfaced per-item rather than dropped. Runs on **Anthropic and Gemini**
  backends (OpenAI batch is a tracked follow-on); a cast whose synth has no batch lane is
  refused with a clear message naming the ones that do. Two ready-made batch casts ship:
  `gemini-batch` (synth Gemini **Pro**) and `anthropic-batch` (synth Claude **Opus**) —
  the tier you reach for offline, where its latency is free. Both declare **`batch = true`**,
  which dedicates them to the batch lane: `batch_submit` takes a batch cast and the
  interactive tools (`consult`/`oneshot`) refuse one — and vice versa — so a big,
  offline-tuned model is never run interactively (slow, expensive) by accident. Mark your
  own cast `batch = true` in `config.toml` (its synth must be a batch-capable backend; the
  per-tool `cast` menu lists the casts each tool actually accepts). Gated by `--no-batch`
  (one flag over every verb). Batch carries its
  own system preamble fit to the offline lane — one complete, self-contained response with
  no follow-up, told to spend on depth — overridable via `[prompts].batch` like the other
  phases. While a batch runs, `job_get` reminds you to go do other work and check back
  rather than wait on it. Lost a handle? `job_list` re-discovers the batches a backend
  still holds (newest first, each with its handle, status, and progress), so a batch is
  never orphaned — defaulting across every batch-capable backend, or scoped to one with
  `backend`. **`attach`** lets you name workspace files to inline as shared context for
  every prompt — "review README.md" with `attach: ["README.md"]`, or `git diff > x.diff`
  and `attach: ["x.diff"]` — so the file's bytes never pass through your own context.
  Text files splice in as text; images (png/jpeg/gif/webp) ride as native image parts
  (with a vision-capable synth model). Paths obey the same workspace boundary as
  everything else (worktrees included); a file outside it, a directory, an oversized
  file, or a binary that isn't a known image is refused with a clear error.
- **`job_wait`** — block briefly and productively for your async work instead of
  blind-polling `job_get`. Fire off consults and batches, do your other work, then `job_wait`
  when you're ready to spend a minute on kaibo: it blocks up to `timeout_secs` (you
  choose — no clamp; interruptible) and returns as soon as something lands, or on a clean
  timeout. By default it hands back what kaibo flags as worth your attention (a job
  finished/failed, a research-limit) plus which consult jobs are still running; pass
  `level: "info"` to also pull the watchable narrative — each kaish command, sweep, and
  milestone the agents ran — into your context. Name batch handles in `handles` to fold a
  one-shot poll of them in too. Nothing wakes you (you choose when to block) and it isn't
  the source of truth — `job_get`/`job_list` are; a clean empty return just means nothing new yet.
  This pairs with launching work in parallel: submit several, do everything else, then
  `job_wait` to merge the outputs.
- **Async consults are watchable again.** A `consult_submit` job now streams its liveness
  (each kaish command, sweep, and milestone) onto kaibo's logging channel — the live
  "watch it work" view a synchronous `consult` always had, restored for the async path.
  It rides kaibo's level convention (Info = the narrative; Warn = "the calling model
  should see this"), so a watching client sees the show and `job_wait` pulls the salient bits.
- **`job_get` / `job_cancel` / `job_list`** — one shared surface to collect, stop, and survey
  *both* kinds of async work (the `job_` prefix self-namespaces even in hosts that
  flatten tool names into one list), told apart by the handle: a batch handle is
  `backend/provider-id`, a consult job is `job-N`. `job_get <handle>` returns a progress/
  status line while the work runs — for a consult job it echoes the latest investigation
  beat (e.g. *currently: exploring …*) with a step count, the same one-liner `job_wait`
  streams, so a poller sees forward motion — and the full result when it lands; `job_cancel <handle>`
  stops it; `job_list` shows everything in flight — your in-memory consult jobs plus the
  batches each backend still holds — each with a ready-to-use handle. One mental model
  for everything you submit. The verbs stay available as long as either `consult` or
  batch is enabled (gated off only when both are). `job_list` trims its batch section to the
  **last 24 hours** by default — a provider keeps months of finished batches and dumping
  them all just burns tokens, while anything older is done and still collectible by its
  handle; it reports how many it hid and takes `all: true` for the full history (true
  orphan recovery). Consult jobs are always shown in full.
- **`view_image`** — vision-capable consultation phases can read an image *file* from
  the workspace into model context (screenshots, diagrams, assets already in the tree).
- **Multi-provider model teams.** Anthropic, DeepSeek, and Gemini natively, plus a
  generic `openai` kind for any OpenAI-compatible endpoint (hosted GPT, local
  llama.cpp / Ollama / Gemma). Configured as **backends** (connections), **casts**
  (named teams), and **roles** (explorer / synth, plus a `vision` capability pin on a
  slot that reads images); a cast can mix families across roles — a cheap local explorer
  with a hosted synth. Built-in casts ship so
  kaibo runs with zero config; `config.toml` merges over them. Precedence:
  per-call > CLI > env > file > built-in, and a missing config file is not an error.
  Your usable casts' names are advertised to the *calling* agent as the per-lane `cast`
  param enum (the tool's schema, with the default flagged) — so a host told "have
  deepseek review this" routes off the roster, and a meaningful name (`local-only`,
  `deep-dive`) reads as intent without the caller opening your config. The startup
  handshake's `## Casts` roster goes further: each line names
  the cast's **answering (synth) model** and tags a batch-only cast `batch`, so a host
  told "ask Gemini Pro" indexes `gemini-batch → gemini/gemini-pro-latest (batch)` — and
  knows it's the `batch_submit` lane — without reading `kaibo://config`.
- **Handshake built to the host's real limits.** Claude Code truncates a server's MCP
  `instructions` at 2048 characters (measured, per-server, hardcoded) — so the resident
  handshake is budgeted to fit, with `## Scope` (the read-only/containment posture)
  moved *above* that fold where a truncating host used to drop it. The kaish shell
  reference leaves the resident text entirely — `run_kaish`'s own description and the
  `kaibo://kaish/*` resources carry it — and each tool description now stands alone
  (some hosts show the model no instructions at all) and opens with the words an agent
  would search for. Under hosts that defer tool schemas to names-only, `consult` is
  pinned resident (`_meta["anthropic/alwaysLoad"]`) so the front door is always legible.
  writing `config.toml`, alongside `kaibo://config` (resolved runtime state) and
  `kaibo://config/example` (annotated template) resources. Secrets are referenced by
  env-var name or key-file path, never inlined. `kaibo://config` flags any per-slot
  tunable the slot's resolved model shape will never send (an `inert_tunables` list —
  e.g. a `thinking_budget` on an effort-only model, an `effort` on a budget-only one),
  so a no-op knob is visible to the operator instead of rendering as if effective.
- **`kaibo://tools` resource — the long-form guide to wielding the tools.** Attachments
  (named-for-the-shell on `consult` vs inlined on `oneshot`/`batch`), picking a `cast`
  and per-call model/backend overrides, the sync↔async pairs and their handle shapes
  (`job-N` vs `backend/provider-id`), and the read-only shell's idioms — including the
  `bash` habits that don't carry over. The tool schemas themselves are now terse and
  point here, so the depth a calling model needs loads on demand instead of riding in
  every agent's startup context (~40% lighter tool descriptions at connect time).
- **`kaibo://prompts` resource — see (and tune) exactly what the models are told.** The
  system preamble each phase receives — the explorer sweep, the `consult` driver,
  `oneshot`, and the offline `batch`/`deliberate` synth — rendered by the *same* code a
  live call runs (any `[prompts]` override folded in), plus how your question is wrapped
  into the user turn. It's an audit surface (what is a model actually reading?) and the
  companion to tuning a preamble: override a phase's role framing globally with the
  `[prompts]` table or per cast with a slot's `preamble`, and the resource shows the
  result. **`kaibo://prompts/<cast>`** goes one step further — it resolves *that cast's*
  framing, its per-slot `preamble`s folded in the way a live call layers them, and
  attributes each phase to whichever set it (cast slot › global `[prompts]` › built-in) —
  so you see precisely what one cast's models are told. Relatedly, a **synth slot's
  `preamble` now frames the offline synth too** — a per-cast voice set on a
  `batch`/`deliberate` cast reaches its `batch_submit` / `deliberate` answers, not just
  the interactive `consult`/`oneshot` phases (previously only the global `[prompts].batch`
  did).
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
- **Repo orientation in the preamble.** Before a `consult`/`explore` investigates,
  kaibo splices the project's layout into the exploring preamble so the model starts
  *knowing* where things are instead of spending its first turns discovering them.
  Small repos get the complete file list; larger ones (over `[orientation]
  full_list_max_files`, default 256) get a depth-limited **directory map** (`dir/  N
  files` lines, `tree_max_depth` deep, default 4) instead of a refused call; a repo
  too large for even that map gets a short note pointing at discovery tools. The map
  is never silently skipped and a big repo is never an error — orientation is an
  enhancement, so its absence just costs the model a few discovery turns it always
  could have taken.
- **Multi-turn sessions** via `session_id`, and optional OTLP/HTTP trace export
  (`[telemetry]`, off by default). Each tool call emits a `tool` span naming the
  tool and a short argument summary; a `run_kaish` span additionally carries
  `kaish.exit_code` and `kaish.output_bytes`, so a trace can distinguish a read that
  *truncated* (exit `3`) at the output cap — and forced narrow re-reads — from one the
  model chose to slice, rather than every script reading as a plain success.
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

### Changed

- **The explorer reads big files in fewer, wider passes.** Its guidance now gives a
  file too large to read whole a first-class strategy — a few wide `sed` spans (a few
  hundred lines each) instead of many tiny slices — so a `consult`/`explore` over large
  sources spends noticeably fewer tool calls gathering the same evidence (measured: a
  broad sweep dropped from 74 read/search calls to 46, reading the same big file in ~13
  wide spans instead of ~22 fifteen-line ones). No behavior change on short files.

### Fixed

- **The advertised cast roster marks the default even when it's set by an alias.**
  Setting `server.cast` (or `--cast` / `KAIBO_CAST`) to a cast *alias* — say `claude`
  for `anthropic` — used to drop the `(default)` tag from the handshake's `## Casts`
  roster and the tools' "Casts ready now" line, because the tag compared the raw string
  against the canonical names kaibo advertises. The default is now resolved before
  comparison, so the right cast is flagged however you named it.

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
  budget. All configurable. Attachments are bounded too: a per-file size cap, plus a
  per-call cap on attachment *count* (64) and *cumulative* bytes (32 MiB), so a stray
  thousand-file glob or many small files summing to an out-of-memory read is refused
  loudly before anything is slurped in.
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
