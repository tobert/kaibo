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
  puts workspace files (under the project root) in front of the investigation — attach
  means *the model sees the bytes*. Text files are **inlined whole** into the
  investigation prompt, lines numbered like `cat -n` so the model cites them by exact
  `file:line`; a file past the cumulative inline budget (`[defaults]
  inline_attach_budget` / `KAIBO_INLINE_ATTACH_BUDGET`, default 256 KiB; `0` = inline
  nothing, the escape hatch for small-context local casts) is instead ordered read WHOLE
  through the model's shell — demoted loudly with its size, never silently dropped. Every
  delegated explorer sweep also gets a read-them-WHOLE directive for the attached files,
  so a sub-agent is never blind to what you flagged as central. An **image** opens via
  the `view_image` tool and therefore needs a vision-capable cast — kaibo refuses one to
  a vision-blind synth up front (the same honest refusal `oneshot`/`batch` give) rather
  than name a file the model could never open. The files just have to live under the
  root the consult reads (a worktree counts).
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
  arguments, plus `attach`: text files the investigator is directed to read WHOLE during
  its sweep (it reads through the shell, so nothing inlines and images are refused —
  attach those to `consult` with a vision cast). Being single-phase, it has no synth
  args, `context`, or `session_id`.
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
  `explorer_model` / `synth_model` (+ `_backend`s), plus `attach` — text files the
  dossier-building explorer is directed to read WHOLE, so their content reaches the
  offline synth through the dossier; the synth itself is a single turn, so no `context` /
  `session_id`. Gated independently by `--no-deliberate`. This is
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
  when you're ready to spend a minute on kaibo: it parks up to `timeout_secs` (you
  choose — no clamp; interruptible) and returns early only when a job finishes or fails (a
  real event), else on a clean timeout — narrative alone never cuts the park short, so a
  single `job_wait` watches a long job without turning into a poll storm. On return it hands
  back a sample of what happened plus which consult jobs are still running. `level` sizes
  that sample, not the timing: the default (`warn`) is the flagged milestones; `level:
  "info"` folds in the watchable narrative too — each kaish command, sweep, and milestone
  the agents ran, coalesced to the most recent `limit` — so a richer level fills the tail
  without ever making the call return sooner (to check in more often, pass a shorter
  `timeout_secs`). Name batch handles in `handles` to fold a one-shot poll of them in too.
  Nothing wakes you (you choose when to block) and it isn't the source of truth —
  `job_get`/`job_list` are; a clean empty return just means nothing new yet. This pairs with
  launching work in parallel: submit several, do everything else, then `job_wait` to merge
  the outputs.
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
- **OpenRouter as a first-class provider.** One `OPENROUTER_API_KEY` now reaches
  every major model family through the built-in `openrouter` backend and cast, with
  reasoning on by default (OpenRouter's unified `effort` param, forwarded verbatim so
  a synth slot can reach past the usual `high` default into `xhigh`/`max` — measured
  live: effort rides through the gateway and bills real reasoning tokens). Setting a
  slot's `effort = "none"` sends OpenRouter's structural disable
  (`{"reasoning": {"enabled": false}}`), so the opt-out doesn't depend on how the
  gateway's effort ladder happens to read the string. The built-in cast defaults to
  **Qwen** (explorer `qwen/qwen3.6-flash`, synth `qwen/qwen3.7-max`) — a family kaibo
  can't reach directly, so the gateway earns its keep with a distinct lineage for a
  genuine cross-family read, rather than re-serving the Gemini/Claude you already have
  keyed. OpenRouter serves no `~qwen/*-latest` router alias, so the cast pins the
  undated family ids — the most rot-resistant Qwen ids available (each tracks the
  newest point-release until the next `.x`). Every OpenRouter call carries an
  explicit prompt-cache breakpoint, so Anthropic-family models behind the gateway
  bill their (large, resident) system preamble at cache-read rates instead of full
  input price every turn — providers whose caching is implicit simply ignore it.
  **Data collection is denied by default**: one OpenRouter slug routes across
  competing upstream hosts whose data policies differ, and kaibo's prompts carry
  your source — so every request pins `provider.data_collection = "deny"` and a
  model whose only hosts retain/train on prompts fails loudly instead of leaking
  quietly. `data_collection = "allow"` on the backend is the explicit opt-in
  (kaibo then emits no restriction; your account settings govern), and
  `kaibo://config` renders the active policy so the posture is always visible.
- **Token usage on the provenance footer.** Every `consult`, `explore`, `oneshot`,
  and direct `deliberate` answer now ends its footer with a `tokens · … in · … out`
  line — the token counts the provider reported for the call, so "what did this cost
  me?" is answered in-band without turning on telemetry. A `consult` sums the synth
  loop *and* every delegated explorer sweep; the cache-read / cache-write / reasoning
  splits ride along only when a provider reports them, so the common line stays lean.
  When a backend reports no usage at all the line is simply omitted rather than
  printing a misleading `0 in · 0 out`. The batch `deliberate` lane, whose synth cost
  lands later on the provider's result, notes the synchronous dossier-build cost in its
  submit acknowledgement. (Counts are exact on the normal path; the rare turn-cap and
  image-resume paths undercount, since the underlying loop yields no usage on those
  exits — noted in `docs/issues.md`.)
- **A container image, built to be COPY'd.** Every release now ships
  `ghcr.io/tobert/kaibo` — multiarch (amd64/arm64), the fully-static binary in a
  distroless, shell-less, **non-root** base, signed and attested by the same
  machinery as the archives. Because the binary links against nothing, the image
  doubles as a one-line install for devcontainers and custom images
  (`COPY --from=ghcr.io/tobert/kaibo:latest /usr/local/bin/kaibo /usr/local/bin/kaibo`),
  and running it directly is one documented mount: your project read-only at
  `/work`. The README's container section has the docker/podman recipes — including
  the two footguns (`-i` is load-bearing for a stdio server; UID mapping for the
  read-only mount).
- **Release pages you can copy-paste from.** Every release now opens with its own
  get-and-verify block: the container pull and `COPY --from` lines, both
  `gh attestation verify` one-liners, and the cosign bundle verification — all
  carrying that release's exact tag, so nothing needs substituting (the README keeps
  the generic `vX.Y.Z` form). It also points out that the `sha256-*` tags on the
  package page are signature artifacts riding alongside the image, not something to
  pull — and the publish order now applies the version tags *last*, so the package
  page's install box always advertises a pullable version tag instead of the
  signature bundle that lands after the image (copy-pasting it got a confusing
  mediaType refusal).
- **Sessions and batch handles now survive a restart (persistence, on by default).** A
  `consult` session — the thread a `session_id` carries — used to live only for the
  server process; it now persists, so you can restart kaibo (or reconnect, or switch
  between the MCP server and a CLI invocation) and pick the same thread back up. Batch
  handles persist too — recovered **on demand** when you run `job_list` (kaibo doesn't
  reattach in the background; the provider stays the source of truth for a batch's state),
  so a long-running batch is never orphaned by a reconnect. kaibo keeps this in a small
  state db under your XDG state dir (`$XDG_STATE_HOME/kaibo/state.db`, else
  `~/.local/state/kaibo/state.db`) — session Q&A turns and batch `{backend, provider-id}`
  records only; background `job-N` handles and exploration reports stay in-memory by
  design. Turn it off with `--no-persistence` / `KAIBO_NO_PERSISTENCE` / `[persistence]
  enabled = false` to run fully in-memory, or move the db with `--state-db <FILE>` /
  `KAIBO_STATE_DB` / `[persistence] path`. If the store can't open, kaibo **fails to start
  loudly** naming that escape hatch rather than silently losing your sessions. The db is a
  convenience layer, safe to delete. See `docs/config.md`.
- **A CLI front door: `kaibo consult` and `kaibo config`.** kaibo now answers without
  an MCP client: `kaibo consult "question" [--cast … --attach … --session … --json]`
  runs the same read-only investigation from the command line — for agents that shell
  out instead of speaking MCP (pi, scripts, CI) and for humans. The answer (with the
  usual provenance footer) goes to **stdout**; progress and logs go to **stderr**, so
  piping stays clean; `--json` emits a structured `{answer, cast, models, usage}`
  envelope for script callers. Exit codes tell the truth: `0` answer, `2` usage/config
  error, `3` containment/setup rejection, `4` consultation failure. `--session NAME`
  rides the persistent store, so a thread started over MCP continues on the CLI and
  vice versa; a stateless consult never touches the db. `kaibo config` prints the
  resolved configuration (what `kaibo://config` shows). Bare `kaibo` still runs the
  MCP server exactly as before — existing client configs are untouched (`kaibo serve`
  is the explicit spelling). The rest of the front door landed alongside:
  **`kaibo oneshot "prompt"`** (a toolless second opinion — reads extra context piped on
  stdin, the `oneshot "…" < notes.md` idiom, plus `--attach`), **`kaibo explore
  "question"`** (a cited survey report), **`kaibo kaish -c 'script'`** (one
  non-interactive command through the read-only sandbox; the process exits with kaish's
  own code), and **`kaibo batch submit|get|list`** over the provider batch lanes (submit
  prints the durable `backend/id` handle; get fetches results or a progress line; list
  shows live + store-recovered handles). Each carries `--json` (its `answer`/`report`
  field is the model's raw words) and the same stdout-is-payload / exit-code contract.
  An interactive REPL is deliberately later.
- **Local batch: run a batch of prompts without a provider batch lane.**
  `kaibo batch submit --local "prompt" …` enqueues a fan-out of toolless prompts to
  kaibo's own state db and prints a durable `local/<id>` handle — **no provider batch
  API needed**, so it works with a local model (or any cast, not just a batch-capable
  one). Drain the queue with **`kaibo batch work`**, a foreground worker you background
  yourself (`&`, `systemd-run`, `cron`): it claims one job at a time and runs each item
  on the job's cast, writing per-item results as they land. Because the queue and the
  results both live in the shared state db, you can **enqueue from anywhere** (CLI or an
  MCP session), run the worker on whichever machine has the compute, and **collect from
  anywhere**: `kaibo batch get local/<id>` (or the MCP `job_get`) renders per-item
  answers/errors, `kaibo batch list` shows local jobs alongside provider batches, and
  `kaibo batch cancel local/<id>` / `job_cancel local/<id>` stops one (a running item
  finishes; the worker checks between items). Attachment content is captured at submit,
  so the files can change before the worker runs. Local batch needs persistence enabled
  (its queue is the state db); it refuses loudly otherwise. Two concurrent workers on one
  db never double-run a job. Coming next: kaibo running the worker itself (an MCP client
  with no shell to background one).

### Changed

- **The README earns its shop window.** A nine-model reader panel (personas played
  by DeepSeek, GLM, GPT, Kimi, Qwen, Gemini, Claude — full study in the PR) read the
  page cold and told us where it lost them, so it changed: a real worked example up
  front — a genuine measured consult of this repo (~4 minutes, **$0.02**, quoted
  with its citations); release-binary download + checksum instructions now that
  v0.2.0-rc.1 artifacts exist (and the unpublished `cargo install kaibo` claim
  replaced with the honest source build); registration rewritten client-generic
  (Codex CLI, Cline, OpenCode — `claude mcp add` is the same stanza's shorthand);
  the async lane (`explore`, `consult_submit` + `job_*`, `batch_submit`,
  `deliberate`) finally documented; pick-a-cast-outside-your-family guidance under
  the casts table; the stale `openai` cast row corrected to `openai-local` ("you run
  the model server; kaibo ships no inference") plus the batch-cast rows; the
  `.mcp.json` example now ships an empty `env` with keys-stay-in-your-shell
  guidance instead of three inline key placeholders; Moonshot/Kimi and Zhipu/GLM
  named as `openai`-kind citizens; the `~author/family-latest` alias claim
  qualified (they exist only for major authors); the network story merged into the
  read-only FAQ; and Backends/Roles/Casts moved below Tools with a one-line "a cast
  is just a named team" opener.

- **The read-only shell under `consult` / `explore` / `run_kaish` speaks kaish 0.12.**
  Native collections land in the toolbox the models drive: list/record literals
  (`xs=[a b c]`, `{port: 8080}`), typed subscripts and slices (`${xs[0]}`, `${r[key]}`,
  `${xs[0:2]}`), `keys` / `values` / `typeof` and `[[ -list ]]` / `[[ -record ]]` shape
  guards, and typed membership (`[[ 443 in ${servers[web]} ]]`). A real `test` builtin
  evaluates POSIX conditions *through the VFS* — where kaibo's no-subprocess sandbox
  used to leave the old `/usr/bin/test` shell-out dead, `test -f path` now works.
  `fromjson` / `tojson` / `fromjsonl` / `tojsonl` bridge JSON and JSONL, `jq -s` is real
  slurp, and redirects work inside `$(...)`. The always-on onboarding now leads with its
  most critical rules and points enumeration at `$(keys …)` / `$(values …)`; `help regex`
  and `help collections` are new one-screen references. All of it arrives through kaibo's
  single-sourced kaish guidance — no new resident cost. `grep -r PATTERN FILE` (a single
  file, not a directory) now actually searches it instead of silently finding nothing, so
  the cheatsheet's old workaround note is gone; a wider sweep of binary-input operands
  (`glob --include`, repeated `--include`/`--exclude`, malformed numeric flags, and more)
  now fail loudly instead of silently misbehaving, matching the read-only sandbox's own
  no-silent-fallback stance; and a runaway recursive script (`$(...)`, shell functions,
  `.kai` sourcing) now fails with a clean "maximum recursion depth exceeded" instead of
  risking a stack overflow.

- **Models read files WHOLE by default, and a truncated giant stages into targeted
  reads.** The explorer, the `consult` driver, and the shared kaish cheatsheet all
  lead with whole-file reads: `cat -n FILE` is the stated first move on any file that
  matters (the old "a *short* file: read it whole" made models classify before daring
  a whole read, then nibble), `grep` is framed as the way to find *which* files
  matter rather than a reading tool, and the `wc -l` pre-probe is gone. The output
  cap stays 64 KiB — ~23K tokens, sized so the worst single turn stays small on a
  128–250K-context explorer — because truncation is now *informative*, not a
  dead end: exit 3 already returns the file's head and tail, and the guidance stages
  the rest as targeted reads (`grep -n SYMBOL FILE`, then a ~1,200-line span around
  it) instead of a mechanical full walk. Fewer, wider turns: every turn re-sends the
  transcript, so one whole-file read beats five slices on both cost and wall-clock.
  (Supersedes the earlier few-hundred-line span guidance, measured at 74→46 calls;
  whole-first goes further. Attached files the caller flagged as central keep the
  read-it-ALL directive — there the full cost is deliberate.)

### Fixed

- **A stalled backend can no longer hang a call overnight.** An interactive
  `consult` / `explore` / `oneshot` now runs under a whole-call wall-clock deadline
  (`call_deadline_secs`, default 1 hour; env `KAIBO_CALL_DEADLINE_SECS`), independent
  of the per-request `request_timeout`. The per-request timeout catches a backend that
  never answers, but not every wedge shape — a stalled response body, or a pooled
  keep-alive to a server that stopped responding, once parked a real consult ~17 hours.
  Past the deadline the call aborts with a clean tool-result error naming it (classed as
  a transient/retryable condition, not a kaibo bug), so your session keeps moving instead
  of waiting forever. Keep the value above your slowest legitimate single completion. It
  bounds the interactive loop tools — `consult` / `explore` / `oneshot` and async
  `consult_submit`. `deliberate`'s direct lane (one long local completion) is bounded
  instead by its synth backend's `request_timeout`, so a slow local model keeps its full
  patience without forcing this ceiling high; the batch lane holds no in-process wait
  (the work runs on the provider's queue).

- **The advertised cast roster marks the default even when it's set by an alias.**
  Setting `server.cast` (or `--cast` / `KAIBO_CAST`) to a cast *alias* — say `claude`
  for `anthropic` — used to drop the `(default)` tag from the handshake's `## Casts`
  roster and the tools' "Casts ready now" line, because the tag compared the raw string
  against the canonical names kaibo advertises. The default is now resolved before
  comparison, so the right cast is flagged however you named it.

### Security

- **Releases are born signed, and you can check.** Every release now carries three
  independently verifiable trust artifacts, produced in public CI with no maintainer
  key to steal: a **cosign keyless signature** over an aggregated `checksums.txt`
  (verify it once and it covers every file it lists — the signing identity is the
  release workflow at that exact tag, witnessed by the Sigstore transparency log),
  **SLSA build provenance** per artifact (`gh attestation verify <file> -R
  tobert/kaibo`, one command), and an **SPDX SBOM** cataloging the exact locked
  dependency tree the binaries were built from. The README's "Verify a download"
  section has the copy-paste invocations, including the identity flags keyless
  verification requires.
- **Read-only is structural, not best-effort.** kaibo compiles in only kaish's
  `localfs` axis — `subprocess` / `git` / `host` / `os-integration` are off, so
  `exec` / `spawn` / `git` / `ps` don't exist in the binary — and mounts the project
  read-only, with an in-memory scratch filesystem for everything else. Reads are
  scope-bounded to `--root` / `--allow-path` (launch cwd by default), enforced after
  symlink and `..` canonicalization. **kaibo still writes nothing into your project.**
  The new persistence store is the one exception to "kaibo writes nothing" — and a narrow,
  guarded one: it lives only at the fixed XDG state path (a model never chooses it),
  refuses to open onto any allowed project tree, and is the single write site a
  source-level guard permits; the shell you drive stays fully read-only.
- **Bounded resource use.** Each kaish script is capped (30 s wall-clock, 64 KiB
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
- **A raced file-swap can't OOM the reader.** Every attachment/image read now carries a
  byte ceiling into the read itself (via the VFS `read_range`, honoured with a real
  `File::take` — no whole-file slurp), sized one byte past the caller's budget. A file
  swapped to something enormous between kaibo's size check and the read stops at that
  ceiling and is refused or demoted by length, where before an unbounded read could pull
  the swapped file whole into memory. Closes the size-swap sibling of the symlink-swap
  above — the timing window is bounded, not raced.
- **Attachment wrappers can't be confused by their own contents.** Neither an attached
  file's *body* nor its *name* can forge the `<file>` wrapper boundary anymore. A body
  holding a `<file>`-tag lookalike — a `</file>` close, a stray opening `<file …>`, or a
  whitespace/case variant — is escaped, and the caller's path (a legal filename can hold
  `"`, `>`, or newlines) is attribute-escaped, so a maliciously-named file can't inject a
  second wrapper. The line between an attachment and the prompt stays unambiguous across
  `oneshot` and batch.

[0.2.0]: https://github.com/tobert/kaibo/releases/tag/v0.2.0
