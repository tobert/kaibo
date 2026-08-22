# Changelog

All notable, user-facing changes to kaibo are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kaibo aims for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`0.2.0` is kaibo's first real public release — the point it's meant for other
people to install and run, not just us. It's also the point kaibo adopts a
pull-request workflow and this maintained changelog. The `0.2.0` entry captures
the feature set as kaibo goes public rather than reconstructing the 0.1
development line; that history lives in the git log, and it's noisy enough
(iterative, exploratory, many small commits) that we may compress it into a
shorter retrospective note later rather than leave it as the implicit "pre-0.2.0"
record. Each later release appends a new section at the top.

## [Unreleased]

### Added

- **`generate` takes input images by digest** — `inputs {"image": "<digest>"}` is
  image-to-image on Stability's `ultra` and `sd3` routes, reusing an image already in
  the store rather than re-sending it.
- **`generate` carries several named input parts**, so the operations that take
  `image`+`mask` or `init_image`+`style_image` are reachable.
- **A media backend with no input-image route refuses a call that carries one**, rather
  than silently generating from the prompt alone and returning an unrelated image.
- **`write_cas` stores an image in kaibo's media store and returns its digest** — the
  deposit half of `read_cas`, and how an image reaches kaibo at all.
- **`write_cas` takes a `path` and reads the file itself**, so an image costs no tokens;
  base64 `content` remains for an image that is not a file.
- **`write_cas` reads the format from the bytes**, so png/jpeg/gif/webp are accepted by
  signature and there is no `mime` to state or get wrong.
- **`[telemetry]` exports the GenAI metrics signal** — token usage, call and phase
  durations, turns per phase, and tool calls per phase, as the conventions name them.
- **`gen_ai.invoke_agent.tool_calls` answers whether a consult driver delegated**,
  split by `synth` / `explorer`, without reading a single trace.
- **`[telemetry] traces` and `[telemetry] metrics`** — per-signal switches, so
  `traces = false, metrics = true` exports kaibo's spend and latency and nothing else.
  No metric can carry prompts, so that posture is safe by construction.
- **`[telemetry] metrics_endpoint`** — omit it and kaibo derives the sibling of
  `endpoint`, the same rule `logs_endpoint` follows.
- **`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` turns telemetry on by itself**, for a
  platform that collects metrics and not traces.
- **`kind = "dashscope"`** — a media backend for Alibaba's wan image family, so
  `generate` can run on a DashScope subscription.
- **kaibo reads the standard `OTEL_*` environment**, including `OTEL_SDK_DISABLED`.
- **`[telemetry] capture_content`** — off by default, so exported spans carry no prompts,
  completions, or tool payloads.
- **`[telemetry] capture`** — export one named attribute without exporting all of them.

### Changed

- **kaibo now fetches a generated artifact's URL when that is how a provider delivers
  it**, over TLS and size-bounded, instead of refusing it — otherwise the operator
  fetches the link by hand with their key.
- **Every outbound request now identifies itself as `kaibo/<version>`** — kaibo's
  traffic previously carried no `User-Agent` at all.
- **Telemetry turns on when `OTEL_EXPORTER_OTLP_ENDPOINT` is set** — safe now that
  content is redacted by default.
- **An explicit `[telemetry] enabled = false` beats the environment**, and
  `OTEL_SDK_DISABLED` beats every source.

### Fixed

- **Bump `kaish-kernel` to 0.14.1** — the explorer's shell no longer drops piped or
  buffered stdin across `read`/`grep`/`cat`, and a loop or `if` that exits early keeps
  what it already printed.

## [0.3.0] — 2026-08-13

### Added

- **The repo map gives each file's size and marks the files one read cannot return
  whole**, so a model stops guessing how much of a file it will get.
- **`[telemetry]` exports the logs signal too** — kaibo's own `tracing` events, which the
  span tree never carried.
- **`[telemetry] logs_endpoint`** — omit it and kaibo derives the sibling of `endpoint`;
  a non-standard one is a startup error naming the key.
- **`deliberate` keeps its dossier in the media CAS, and `dossier` reuses one** — hand the
  digest to a second cast to reason over the same evidence for one synth's price.
- **A refused dossier write never fails the deliberation** — you lose the record, not the
  answer.
- **Batch deliberations persist their handle**, labelled with the dossier's address, so
  `job_list` leads back to the evidence after a restart.
- **`kaibo deliberate`** — a batch cast prints the durable handle and exits; a direct cast
  runs in the foreground ([#82](https://github.com/tobert/kaibo/issues/82)).
- **`kaibo batch cancel <handle>`** — the wording says "requested", because a provider
  finishes what is already in flight.
- **`kaibo generate`** renders into the artifact store; a deferred render polls in the
  foreground rather than returning a handle nothing could collect.
- **`kaibo cas read <digest>`** — metadata to stderr, content to stdout, binary as raw
  bytes; `read` is the only verb.
- **kaibo warns when the media CAS sits on a filesystem that will not survive the
  container** — it proceeds, and `[cas] backing` in `kaibo://config` reports the finding.
- **`read_cas` — read a stored artifact back by digest.** Metadata always leads, and an
  image up to 5 MiB arrives whole and viewable.
- **A consult can hand you bulk output as an artifact instead of spending your context on
  it** (`save_artifacts: true`) — opt-in via `[artifacts] enabled`, with fixed limits.
- **A second media kind: `openai-images`** — hosted OpenAI image generation and a local
  `sd-server` through one backend kind.
- **kaibo can generate images**: the `generate` tool renders a prompt through a cast's
  `image` slot into a content-addressed media store, never inline bytes.
- **The media CAS has a lifecycle, and it follows persistence** — durable on disk while
  persistence is active, in-memory without it, `[cas] enabled = false` off entirely.
- **Artifact retrieval is operator-surface only** — the inner model team never sees the
  CAS, so one project's team cannot enumerate another's artifacts.
- **Model listings show each model's output ceiling** beside the context window — size a
  synth slot's `max_tokens` from it.
- **The explorer can hand whole files to whoever reads its report** — an `attach` tool
  routes real bytes, images included, into the driver's context or the dossier.
- **Traces say how each model turn ended** — `run_phase` spans carry
  `gen_ai.response.finish_reason`, tool-loop turns included.
- **`effort = "max"` reaches hosted GPT-5.6** — the wall was rig's, removed by rig 0.41.
- **New MCP resource `kaibo://config/guide`** — the configuration manual embedded in the
  binary.
- **CLI subcommands export telemetry too** — every export flushes before the process
  exits, so a short-lived run no longer loses its spans.
- **Model listings mark which models reason**, read from the provider's own catalog, so
  picking a thinking model needs no guesswork and no API call of your own.

### Changed

- **Reasoning is on by default on every OpenAI-compatible backend** — kaibo sends
  `reasoning_effort`, so a thinking model behind a gateway or a local server no longer
  answers thin.
- **`thinking_style = "off"` turns reasoning off on any provider** — the escape hatch for
  a server that rejects an unknown parameter instead of ignoring it.
- **kaibo runs on kaish 0.14**, which stopped holding state for its embedders — the shell
  kaibo hands a model exposes less than before, not more.
- **kaibo stopped telling models to quote a `sed` range** — kaish 0.14 made a bare comma
  ordinary, so the false reason went and the quoted examples stayed.
- **kaibo speaks MCP through the released rmcp 3.1.2**, off the 3.0 beta it shipped on.
- **The built-in deepseek and anthropic synths pin `max_tokens = 32768`** — reasoning was
  consuming half the completion budget before the answer started.
- **kaibo's models work from a role, not a job description** — every preamble opens on who
  the model is on kaibo's team.
- **The explorer preamble anchors its toolset** — the request's tool list is stated as
  complete, aimed at flash-tier explorers inventing `run_*` tool names.
- **kaibo's prompts are written in plain, literal English** — most of the models kaibo
  drives are not English-first.
- **kaibo's models read a wide span around a grep hit in a large file, and run `file` on an
  unfamiliar one** — whole-file reading stays the default everywhere else.
- **`oneshot` and `deliberate`'s direct lane are one literal request** instead of a toolless
  pass through the managed loop.
- **Batch treats `effort` as a floor, not an override** — a slot asking deeper keeps its
  rung; a shallower one is still lifted.
- **`effort = "none"` survives the batch lane** — "off" is not a depth, so a fan-out with
  reasoning off stays off.
- **kaibo no longer advertises a tool no configured cast can staff** — a startup warning
  names the cast shape that would bring each tool back.
- **kaibo refuses to start when nothing is left to advertise** — the staffing road to an
  empty surface now exits non-zero like the all-flags-off road always has.
- **`docs/config.example.toml` is leaner, and `docs/config.md` reads as a reference
  manual** — explanation moved from the template to the guide.
- **The example config documents four knobs it had been missing** — `[persistence]`,
  `[orientation]`, `job_capacity`, and `inline_attach_budget`.
- **The tool is plain "kaibo" now** — the 解剖 kanji appears only in the README's `## Name`
  section.

### Removed

- **`docs/issues.md` and `docs/devlog.md` are no longer in the repo** — open work and the
  shipped-work narrative are tracked elsewhere; pull requests and this changelog are the
  public record. Their history stays readable in git.
- **The `kaibo://cas/<digest>` MCP resource** — never in a release, replaced by the
  `read_cas` tool; a stale request answers with the migration.

### Fixed

- **A complete answer from a vLLM-served model no longer arrives as a provider error** —
  vLLM sends both `reasoning` and `reasoning_content`, which rig-core cannot decode, so
  kaibo repairs the body before it is parsed.
- **kaibo told models the wrong exit code for a refused write.** Six descriptions said
  `126` meant blocked; a refused write exits `1` saying `permission denied: filesystem is
  read-only`, and an external command exits `127`.
- **A model calling a tool that does not exist no longer throws away the investigation** —
  it gets a tool result naming the real toolset and corrects itself.
- **`--no-<tool>` gates work again over MCP.** Since the rmcp 3.0 upgrade a disabled or
  unstaffable tool was still listed and still callable.
- **A new Claude Code sees kaibo's tools again** — the client rejects a list result missing
  `ttlMs`/`cacheScope`, which rmcp leaves unset, so kaibo fills them itself.
- **`--include-report`'s help described the wrong stream** — the report goes to stderr, so
  a script that piped stdout lost it silently.
- **`--cas-max-bytes` was documented as a "soft cap"** — a write past it is refused and
  nothing is evicted.
- **`kaibo batch` advertised "half price"** as a fact kaibo does not control — it says
  "typically" now.
- **One malformed tool call no longer discards the investigation** — kaibo sends that
  request twice more before failing the call.
- **A batch result no longer under-reports silently** — any submitted `custom_id` a
  provider never returned is a loud per-item failure.
- **A consultation that produced no answer no longer comes back as a successful empty one**
  — with evidence gathered kaibo asks once for the write-up, and with none it fails loudly.
- **An empty-answer failure reads as a retryable model outcome, not a kaibo bug** — the
  guidance invites a retry or a different cast.
- **A vision model still sees the image after the rig 0.41 upgrade** — `view_image` hands
  rig a declared image block instead of a JSON envelope.
- **A tool failure is readable by the model again** — rig 0.41 began masking tool errors
  with a generic "the tool failed".
- **A reasoning `effort` the provider client cannot accept is refused up front**, naming
  the cast, role, backend, model, and the rungs that path does take.
- **`[defaults] synth_effort = "xhigh"` no longer quietly breaks every Gemini cast** — the
  docs carry the real per-provider ladders.
- **`effort = "none"` on a DeepSeek slot actually turns reasoning off** — kaibo sends the
  structural disable instead of "enabled, zero effort".
- **An `effort` that lands nowhere is said out loud** — a startup warning names the cast and
  slot, and `kaibo://config` lists it under `inert_tunables`.
- **`kaibo://config` reports inert tunables the way the request is actually built** —
  `thinking_style`, `[defaults]`-sourced effort, and batch-lane slots included.
- **The startup warning and `kaibo://config` can no longer disagree about an effort** — one
  shared rule, and the warning says which thing happened.
- **Corrected configuration-manual claims that did not match the code** — the load-bearing
  one: a synth slot's `preamble` does reach the offline `batch` and `deliberate` phases.
- **The state-db-collides-with-project-tree refusal now also names `--root`/`--allow-path`**
  — usually the right fix.
- **`kaish.output_bytes` lands as an OTel int, not a string.**

## [0.2.0] — 2026-07-29

### Changed

- **MCP SDK upgraded to rmcp 3.0.0-beta.5.** kaibo now builds against the
  3.0 beta line of the Rust MCP SDK, fixing a "connection closed" failure
  where some MCP clients (including Crush) could not complete the handshake
  against the prior 0.16 release. The upgrade adopts the SDK's new type
  names (`Content` → `ContentBlock`, `RawResource` → `Resource`, etc.),
  the MRTR result enums (`ReadResourceResponse` / `GetPromptResponse`),
  builder-pattern constructors for now-`#[non_exhaustive]` structs
  (`ServerInfo`, `Implementation`, `PromptArgument`,
  `ProgressNotificationParam`), and the `RequestMetaObject` split for
  request `_meta`. No user-facing tool behavior or wire shape changed.

- **Configure guidance now calls out host-agent sandboxes.** `/kaibo:configure`,
  `kaibo configure`, and the setup docs now tell agents that kaibo needs outbound network
  to configured model APIs, writable access to its XDG state dir for persistent MCP
  sessions/batch handles, and XDG data access for the media CAS when artifact-producing
  tools are enabled. The guidance also names the per-client split option so Codex,
  Claude Code, and other agents can keep session history or generated artifacts separate,
  and notes that Codex commonly needs explicit sandbox grants where Claude Code usually
  starts local MCP servers with ordinary home-dir access.

### Added

- **Homebrew install.** `brew install tobert/kaibo/kaibo` now installs kaibo from
  the `tobert/homebrew-kaibo` tap, pulling the prebuilt, signed release binary for
  macOS (Apple Silicon + Intel) and Linux (arm64 + x86_64). Installing through
  Homebrew also sidesteps the macOS Gatekeeper quarantine prompt a browser download
  triggers — `brew` doesn't attach `com.apple.quarantine`, so the binary runs without
  the "cannot verify the developer" dialog. A `scripts/bump-tap.sh vX.Y.Z` helper
  refreshes the tap formula from a published release's checksums as the last step of
  cutting a release.

- **`list_models` (MCP tool) and `kaibo models` (CLI) — read-only model discovery.**
  Ask kaibo what models a configured backend's provider actually serves instead of
  hand-rolling a `curl` with the right auth header: it queries the backend's real
  `/models` endpoint (DeepSeek, OpenRouter, Anthropic, Gemini, or any OpenAI-compatible
  endpoint) with kaibo's already-configured auth and base URL, paginating where the
  provider requires it. Output is normalized fields (id, display name, context window,
  created, and — where the provider advertises it — per-token pricing, kept as the
  provider's own strings rather than rounded floats) plus the provider's raw per-model
  object. Omit `backend` to sweep every configured backend at once; `--json`/
  `--no-list-models` follow the same conventions as every other tool, and the MCP tool
  now also returns the `--json` envelope as `structured_content` alongside the prose, so
  a machine caller doesn't have to parse the human-readable listing. No cast, no model
  in the loop — a pure operator/config query, like `kaibo://config` or `batch list`.
- **GPT-5.x is now usable behind OpenAI-compatible gateways via `wire = "responses"`.**
  A new per-backend `[backends.<name>]` knob (`openai` kind only) picks the interactive
  request shape explicitly: `"responses"` for rig's Responses client, `"chat"` for
  OpenAI-compatible Chat Completions. It's optional — unset still infers the shape from
  the endpoint, exactly as before — and exists for a gateway/proxy that implements the
  Responses API at `/v1/responses` (verified against a real gateway) without sitting at
  OpenAI Platform's own URL: without it, current GPT-5.x reasoning models reject Chat
  Completions' `max_tokens` outright. `wire` never affects batch eligibility, which stays
  Platform-only.
- **Hosted OpenAI Platform backends now use the Responses API for interactive GPT calls.**
  A `kind = "openai"` backend pointed exactly at `https://api.openai.com/v1` can run
  current GPT-5.6 models through `oneshot` and `consult`, including image attachment and
  `view_image`; generic/local OpenAI-compatible backends stay on the Chat Completions path.
- **OpenAI joins the offline batch lane.** A hosted OpenAI Platform backend can now staff a
  `lane = "batch"` synth, so `batch_submit`/`job_get`/`job_cancel`/`job_list` and
  `deliberate` all reach GPT — same kaibo semantics as the Anthropic and Gemini lanes
  (toolless, max thinking, half price, a durable `backend/provider-id` handle). OpenAI's
  batch protocol is the file-based one: kaibo builds the requests as JSONL **in memory** and
  uploads them as a file in *your* OpenAI account, then downloads the results — it writes
  nothing locally, and deletes nothing (that input file is the only record of what you
  submitted, and OpenAI expires batch outputs itself after 30 days). Batch capability is now
  judged per **backend** rather than per provider kind, because only OpenAI Platform serves
  `/v1/batches` — a `lane = "batch"` synth on a local OpenAI-compatible server is refused at
  config load with the fix named, instead of 404ing at submit time.
- **The gemini backend can now dial a Gemini-API-compatible gateway/proxy.** Setting
  `base_url` on a `kind = "gemini"` backend used to be a load error; it now works the
  same way it already does on the anthropic kind — unset still resolves to Google's
  own endpoint, set it points both interactive (`consult`/`oneshot`) and batch calls
  at your gateway instead. The value is a host root (matching rig's own client and
  the anthropic kind's convention), not a versioned path — kaibo appends the
  `/v1beta` version segment the batch lane needs on its own.
- `docs/openai-api-plan.md`, a living plan for making hosted OpenAI a first-class kaibo
  backend and clarifying that kaibo's OpenAI model calls use OpenAI Platform API keys, not
  Codex subscription entitlement.
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
  model chose to slice, rather than every script reading as a plain success. Each phase's
  `run_phase` span carries `gen_ai.request.thinking` — the exact reasoning/sampling blob
  it shipped (Gemini's `thinkingLevel`, an Anthropic adaptive `effort`, a DeepSeek
  `reasoning_effort`) — so a trace shows *whether and at what depth* thinking was on, the
  wire truth behind each `chat` span's `reasoning_tokens`. The **batch** lane is equally
  legible: a `batch_submit` span records the same `gen_ai.request.thinking` (the forced
  `BATCH_EFFORT` shape) plus the `model` and item count, so a batch fan-out shows what it
  shipped even though it's assembled outside `run_phase`.
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
  exits.)
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
  loudly** naming that escape hatch rather than silently losing your sessions. kaibo never
  deletes that db — it holds answers you paid for, so moving it aside is always your call.
  See `docs/config.md`.
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
- **`kaibo configure` and `kaibo example-config`** round out the CLI front door with
  the two setup surfaces that were MCP-only: `kaibo configure [goal]` prints the same
  guided "set up my models" walkthrough as the `configure` MCP prompt (`/kaibo:configure`
  in Claude Code), and `kaibo example-config` prints the annotated `config.toml`
  template the `kaibo://config/example` resource serves — `kaibo example-config >
  ~/.config/kaibo/config.toml` is a one-line starting point. Both reuse the exact same
  text/template the MCP surfaces render, so the two front doors can't drift apart.
  kaibo's CLI still never writes `config.toml` itself — `configure`'s roster-design
  guidance is unchanged from the MCP prompt, only its opening and closing steps differ
  (pointing at the new plain subcommands instead of MCP resource URIs a CLI-only caller
  can't necessarily reach, and skipping the "reconnect the server" step since a
  one-shot CLI invocation re-reads `config.toml` fresh every time).
- **An `anthropic`-kind backend can now set `base_url`.** Unset still dials rig's
  built-in `https://api.anthropic.com`; set, it points the Anthropic Messages API
  wire protocol at a compatible gateway or proxy instead (a corporate LLM
  gateway, a Tailscale-fronted endpoint, etc.) — the same escape hatch the
  `openai` kind already had, extended to the one other kind whose wire protocol a
  proxy can plausibly reimplement. Every other keyed kind (DeepSeek, Gemini,
  OpenRouter) still rejects `base_url` at load; rig fixes those endpoints. See
  `docs/config.md`.

### Changed

- **`--allow-path` is additive now — it no longer costs you the launch cwd.** Adding a
  tree used to *replace* the zero-config workspace: `--allow-path /extra` with no
  `--root` dropped your project out of the allowed set entirely, so every call that
  omitted `path` failed with "no default root". The flag widens the boundary and never
  narrows it. Naming a `--root` is unchanged — that's you choosing the project, so the
  cwd isn't added beside it — and the new **`--no-cwd`** (`KAIBO_NO_CWD`,
  `[server] infer_cwd = false`) gives back the strict reading: the allowed set is exactly
  what you named, and every call passes its own `path`. If you were relying on
  `--allow-path` alone to *narrow* scope, add `--no-cwd`.
- The README explains `consult` up front now — who acts, in what order — instead of
  leaving the mechanism scattered across four sections.
- Hosted GPT examples now use the current GPT-5.6 family: a fast/tool-capable
  `gpt-5.6-luna` explorer paired with `gpt-5.6-sol` as the flagship consult synth, plus
  `gpt-5.6-terra` as a balanced alternative.
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

- **The README documents the CLI.** The `kaibo consult`/`oneshot`/`explore`/`kaish`/
  `batch`/`config` subcommand surface shipped in earlier PRs (#77, #78) with zero
  README coverage — a reader had no way to discover it existed. A new **CLI** section
  (an MCP-tool-to-subcommand table, the stdout-is-the-answer/stderr-is-everything-else
  + `--json` + exit-code contract, and where `--session`/batch-handle state actually
  lives on disk) now sits right after Installation; the Introduction and the
  container-image section point at it too (the same image runs as a CLI — drop `-i`,
  append a subcommand). `consult_submit`/`job_wait` and `deliberate`'s direct lane are
  the one gap: their job handles are in-memory on a long-running server process, so a
  one-shot CLI invocation would exit and take the job with it before it could be
  collected — tracked as [#82](https://github.com/tobert/kaibo/issues/82).

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

- **The read-only shell now speaks kaish 0.13.** A bug-fix release, inherited for
  free (no kaibo-side change needed): `wc`/`xxd` now refuse invalid UTF-8 loudly
  instead of lossy-decoding it into wrong char/word counts or corrupt bytes; a
  failed `$((...))` arithmetic expansion propagates its real error instead of
  silently splicing in an empty string; `push` accepts a bracket-path target
  (`push services[web][tags] item`), not just a top-level name; case patterns
  accept dash/plus bare words (`--`, `-h|--help) ...`); `printf`/`awk`'s `%Ns`
  width now pads by display width, so a CJK or emoji argument lines up instead
  of under-padding; and several heredoc/arithmetic/quoting parser fixes (nested
  `${X:-${Y}}` defaults, arithmetic inside command substitution, an apostrophe in
  a heredoc body no longer poisoning later arithmetic).

### Fixed

- **A keyless Gemini backend now authenticates against an ambient-auth gateway.** With
  `base_url` on the gemini kind (above) pointed at a Gemini-API-compatible gateway that
  gates access by network identity rather than an API key, a `key_optional` Gemini backend
  used to fail every *interactive* call (`consult`/`oneshot`/`explore`): kaibo sent its
  non-empty `"no-auth"` placeholder, rig put it in the `?key=` query param, and the gateway
  forwarded that dummy key upstream to Google, which rejected it (`API_KEY_INVALID`). A
  keyless Gemini backend now resolves to an *empty* query key, so kaibo emits a bare `?key=`
  the gateway accepts via its own identity. The placeholder is transport-shaped: header-auth
  kinds (OpenAI/Anthropic — and Gemini's own batch lane, which sends `x-goog-api-key` as a
  header) keep the non-empty bearer stand-in a keyless server ignores. Real Google is
  unaffected — it requires a real key, which always resolves ahead of the placeholder.
- **The README described the state db wrongly** — a "convenience cache" holding "no file
  contents", when a named session holds your questions and answers, and deleting it drops
  your batch handles.
- **The README implied kaibo picks an outside model family for you** — it can't tell what's
  asking, and the default cast is `anthropic`, so a `cast`-less call could be Claude
  reviewing Claude.
- The FAQ drops the dollar figures (pricing moves, and cost swings by cast), stops
  promising a budget ceiling kaibo doesn't have, and answers what actually leaves your
  machine.
- **A Gemini slot's reasoning-effort setting now actually reaches Gemini.** kaibo's
  Gemini casts (the default `gemini` synth, the `gemini-batch` Pro synth, the Flash-Lite
  explorer) all name 3.x-line models, but the thinking-knob classifier recognized only
  the bare `gemini-3-…` form — so the `-latest` aliases and the dotted 3.x ids (e.g.
  `gemini-3.1-pro-preview`) fell through to the retired 2.5-era `thinkingBudget` path,
  which pins depth to a fixed number and silently drops the per-role `effort` lever.
  Setting `effort = "low"` (or `"high"`, `"max"`, …) on an interactive Gemini slot had no
  effect. kaibo now routes every Gemini id to `thinkingLevel` — the knob Google's current
  API documents across the whole 3-line — so an interactive Gemini slot's configured
  effort is the reasoning depth it runs at. (The batch lane still runs at its own forced
  max effort, by design. kaibo targets the Gemini 3-line and newer; the legacy 2.5 budget
  knob is no longer modeled — a pre-3.x id fails loud rather than silently mis-shaping.)

- **A Gemini or Anthropic-adaptive slot with a large `thinking_budget` no longer fails to
  load.** The `thinking_budget < max_tokens` load check (Anthropic 400s on an inverted
  pair) was gated on the provider *kind*, so it also rejected slots whose model sends no
  budget at all — a Gemini slot (takes a `thinkingLevel`) or an Anthropic *adaptive* slot
  (takes an `output_config.effort`) with, say, `max_tokens = 4096` and the default
  `thinking_budget = 8192` was refused even though that budget never reaches the wire. The
  check now gates on whether the resolved model shape actually *sends* a budget, so an
  inert `thinking_budget` no longer blocks a valid config.

- **A truncated batch answer no longer masquerades as a finished one.** When a batch
  item hit its output-token budget mid-response — most often a big attached-file review
  where max-effort thinking spends the whole budget before the answer is written — kaibo
  presented whatever text came back as a clean result, with no signal it was cut off. A
  caller skimming for findings could mistake a truncated reasoning fragment (no verdict,
  no structure) for "the model found little." kaibo now reads each item's finish reason
  (Gemini `finishReason`, Anthropic `stop_reason`): a clean finish is unchanged, but a
  truncated or policy-halted one is flagged — the partial text is kept under a loud
  `⚠️ INCOMPLETE` banner naming the reason, and an item that produced no answer at all
  becomes an honest per-item failure instead of a blank success. The batch preamble also
  now steers the model to write its conclusion first (reasoning and answer share one
  output budget), and the example config documents raising a batch cast's `max_tokens`
  for long reviews. (GH #75)

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

[Unreleased]: https://github.com/tobert/kaibo/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tobert/kaibo/releases/tag/v0.2.0
