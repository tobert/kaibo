# AGENTS.md — kaibo

Kaibo is a stdio MCP server that provides an assistant agent **for other agents**.
It augments a calling agent (Codex, Claude, Gemini, local agents, etc.) with a team of
models, lending one kind of help — *consultation*: grounded, cited, read-only answers
about a codebase. The team
*perceives* what fuses into its reasoning (image input today — `view_image` and image
attachments on the model-driven tools; more modalities as the models gain them), and —
when a cast carries an `image` slot — *produces*: the `generate` tool writes artifacts
into kaibo's own media CAS (content-addressed, provenance-recorded, retrieved by the
operator via `kaibo://cas/<digest>`), **never into the project**. Recording and emitting
are always a specific mediated tool over kaibo's own store, not a general write path;
see the read-only invariant.

## For agents working here

- Keep status explicit. If you change direction, say what changed and why. If a tool or
  sandbox blocks work, report the exact blocker and the safe next option.
- Prefer a worktree for PR work. Use `~/src/wt/<repo>-<topic>` for local worktrees.
- Keep `signoff.md` as local, ignored session memory when useful; melt durable parts into
  checked-in docs before they go stale.
- Use a little 日本語（にほんご）when it fits; add ふりがな for kanji.

**Consultation: one primitive, three tools.** The primitive is `run_phase`
(`consult.rs`): a model + preamble + an *injected toolset*, run as a bounded tool
loop. Each consultation tool is that loop wearing different clothes:

- **`consult`** — the synthesis agent with `{run_kaish, explore′}` + optional caller
  `context`: it reads precise spans directly and delegates broad sweeps to a cheap
  explorer sub-agent, then answers. No rigid explorer→synth hand-off; the model
  chooses. Supplied context is trusted starting evidence — it investigates for
  *more*, not to re-verify. The `explore′` sweep (`report_preamble`) lives *inside*
  this loop now; it is not a standalone tool.
- **`oneshot`** — the synthesis agent with **no tools**: the caller owns the context, so
  it's one upstream request, prompt in / answer out, no codebase access. The thin
  counterpart to `consult` — and the door to a model outside the caller's family.
- **`run_kaish`** — drive the read-only kaish shell directly, no model in the loop.

Both model-driven tools name their cast + answering model(s) in a provenance footer
(`with_provenance` in `server.rs`), so a cross-model study sees which model answered.

A `--no-<tool>` capability flag per tool gates the surface (all on by default; `consult` also
gates `consult_submit`, `batch` gates `batch_submit`, `generate` additionally needs the
media CAS on, and the `job_*` verbs follow their live producers — a deferred `generate`
mints a `job-N` like the other producers). A tool also needs a cast that can **staff**
it, or its route is dropped;
a server left with nothing advertised is refused at startup, by either road. See
`CAST_ENUM_RULES` and `live_tools` in `server.rs`, and "Tool gating" in `docs/config.md`.
Multi-provider over `rig-core`: a **`ProviderKind`** is the wire protocol (keyed
Anthropic / DeepSeek / Gemini / OpenRouter, plus **`openai`** for any OpenAI-compatible
endpoint). A **`[backends.<name>]`**
(`config.rs`) is a *named connection* of a kind with its own base URL and key source —
so two `openai` backends (hosted GPT and a local Gemma/llama.cpp server, say) can be
live at once. A **`[casts.<name>]`** is a model team mapping each reasoning role
(`explorer` / `synth`, with a `vision` pin where a slot reads images) to a
`"backend/model-id"`, freely cross-backend, so one cast can pair a cheap local explorer
with a hosted synth; a call picks its team with the `cast` arg. Backends and casts come from a built-in registry merged under an XDG
`config.toml`, `KAIBO_*` env, then CLI flags (precedence: per-call > CLI > env > file >
built-in); a missing config file is a non-error. See `docs/config.md`, and
`docs/casts.md` for the backends/casts design rationale. kaibo never modifies the
project and cannot run external commands.

## Invariants — do not weaken without a failing-first test

- **Read-only is the product.** Enforced in `src/sandbox.rs` by four *structural*
  levers — there is no hardcoded denylist: (0) a minimal feature surface (only the
  `localfs` axis; `subprocess`/`git`/`host`/`os-integration` are OFF, so
  `exec`/`spawn`/`kill`/`git`/`ps` are never compiled in), (1) a read-only mount
  (every write/delete/`mkdir`/`touch`/`dd of=` is refused at the VFS layer), (2)
  `MemoryFs` at `/` (paths outside the project land in ephemeral scratch, never on
  disk), and (3) external commands disabled. The `Blocked` wrapper survives only for
  the config-driven `[sandbox].disable_builtins`, which can make the box *stricter* —
  see the module doc-comment. Any change here keeps `tests/sandbox.rs` green and adds
  a test that can fail. **The shell writes nothing and kaibo never touches the project**
  (no kaish write path; the four levers unconditional). Read-only is scoped to what the
  *model can steer* — kaish's VFS never sees kaibo's own state. kaibo keeps handler-side
  state (sessions + batch handles, the latter recovered on demand via `job_list`) only
  through the persistence store (`src/store.rs`) at a **fixed XDG path no model controls**:
  refused if it resolves into any allowed tree (`tests/store.rs`), and written via turso
  plus a **blessed `create_dir_all`** that `tests/no_write_path.rs` carves out. That store
  was the one deliberate write surface; the **media CAS** (`src/cas.rs` — generated
  artifacts) is the second, under the same discipline: fixed XDG path, refused if
  it resolves into an allowed tree, its own blessed marker. The CAS is safe by *shape*, not
  policy — **the address is the content hash**, so its API has no destination-path
  parameter for a model to aim; it is write-only (`create_new`; no unlink/truncate/rename,
  so an edit is copy-on-write) and never mounted into kaish, whose read side would
  otherwise enumerate every project's artifacts. `tests/no_write_path.rs` blesses only
  individually **marked lines**, each pinned to its file *and to the specific call it
  excuses*, with the tree-wide total pinned to an exact count. So a new write site cannot
  ride in behind an existing marker, and a marker cannot excuse a call it was not blessed
  for. That test is the source for which lines are blessed and how many; read it there
  rather than restating it here. Every other `std::fs` mutation in `src/` fails the guard.
  Anything further that must *record* or *emit* is its own individually-gated mediated
  tool, never a general filesystem escape hatch or a loosening of the four levers.
  Read-*scope* is also bounded: every call's path must canonicalize (symlinks,
  `..` resolved) into the allowed set (`--root` / `--allow-path`, launch cwd when unset).
  Enforced in `server.rs::resolve_root`, with tests in `tests/containment.rs`.
- **Operator surface vs. the model team — do not blur.** The trust model the whole
  design rests on: the model-facing shell cannot modify the world (unconditional, above),
  and kaibo acts only on *kaibo's own* things (the XDG state dir and media CAS) through
  individually-gated, narrow surfaces — the project untouchable from every path. The line
  this draws: kaibo's **tools** (the MCP verbs, the CLI subcommands) are the **operator**
  surface — the client model / CLI caller sees kaibo's own state and config (sessions,
  batch handles, `kaibo://config`) because it is the operator's proxy. The **inner model
  team** (explorer/synth) never does: it works one question, and kaibo state spans
  projects, so surfacing it to a cast is a cross-project leak. Concretely — **model-facing
  kaish never grows `jobs`/`ps` or any kaibo-state builtin**; operator job/state visibility
  lives only in the tools (MCP, CLI, later the REPL), never in the shell we hand a model.
- **Persistence engine (turso) — do not weaken.** Pure-Rust `turso`, **exact-pinned**
  (`=0.7.x` — `.db-tshm` format and API both drift between releases) with
  **`default-features = false`** (defaults pull mimalloc's global-allocator hijack + fts)
  and **`sync` off** (it materializes a network stack, breaching stdio-only + the
  aws-lc-free tree). **One DB-open helper**, MP-WAL hardwired on for 64-bit Unix: a mixed
  MP/non-MP open of one file **silently loses acknowledged writes** (turso's own guard
  against it is unenforced), so any second open-site is a data-loss bug — crash over
  corrupt. Windows / non-64-bit-Unix run single-process; the concurrent-open case
  (`SingleProcessLocked`) is the **one** carve-out to loud-fail-at-startup — it warns and
  degrades to in-memory sessions (so an auto-restarting MCP client can't crash-loop),
  surfaced in the log and as `persistence.active = false` in `kaibo://config`; every other
  open error stays fatal. Connect-per-operation, never a shared `Connection`. See
  `src/store.rs`, `main.rs`.
- **stdio only.** kaibo can read a filesystem, so it must never bind a socket.
- **kaish is `!Send`.** The kernel runs on a dedicated thread behind `KaishWorker`;
  rig tools require `Send` futures. Don't hold the kernel across an `.await`.
- **TLS is rustls + ring, no aws-lc / no OpenSSL.** The whole tree must stay free of
  `aws-lc-sys` and `openssl-sys` (both are C/cmake) so we ship fully static single-
  file binaries — musl links with nothing but a Rust toolchain. reqwest is built
  `rustls-no-provider` and we install ring at every client build site (`src/tls.rs`);
  `tests/tls.rs` proves a real client builds, and `cargo tree -i aws-lc-rs` must come
  back empty. The trap: enabling reqwest's `rustls` feature (directly, or via
  rig-core's default features, or otlp's `reqwest-rustls`) silently re-pulls aws-lc —
  the Cargo.toml comments mark each of those three off. See **Build & release**.

## Working here

- **TDD.** Tests that can and will fail. The sandbox boundary gets failing-first
  tests — and we prove they actually work (e.g. mount the project writable with
  `LocalFs::new` instead of `read_only`, watch the write-denial tests fail).
- **Tests start from a blank environment.** A test that spawns the kaibo binary
  clears the child's env (`env_clear()`) and rebuilds only what it means — temp
  `HOME` and XDG roots — never an enumerated `env_remove` list, which rots the day
  a new variable ships. In-process tests inject env through `load_with`'s map, not
  the process environment. So a developer's keys and `KAIBO_*` overrides can never
  change what a test observes (`tests/mcp_stdio.rs` is the model).
- **`docs/sandbox-probes.md` is the read-only/containment audit runbook.** The
  `cargo test` suites are the continuous guard; that doc is how we *live-test* the
  shipped boundary now and then (write/external/read-escape/env/path batteries via
  `run_kaish`, plus an optional model-driven pass). It's framed as **defensive** work
  — auditing our own claims — and routes adversarial framing to a **local** cast so a
  remote classifier never sees it. Re-run it before a release; stamp the "Last run" line.
- **Model loops are tested offline.** A scripted `CompletionClient` in
  `src/test_support.rs` (`#[cfg(test)]`) drives the *real* consult loop with no
  network — delegation→report aggregation, session replay, turn-cap recovery. It's
  **content-driven, not consumption-ordered**: a responder branches on the inbound
  `CompletionRequest` (preamble, transcript, `tool_choice`) keyed by model id, the way
  a real model reads the whole request each call. That's deliberate — rig runs a
  turn's tool calls with `buffer_unordered`, so a queue-pop mock ("Nth call → Nth
  step") would race the day a turn emits two tool calls or someone bumps concurrency,
  and the finalize replay (`tool_choice::None` ⇒ answer-now) falls out for free.
  Two primitives — response strategy + a request log — so new cases (multi-sweep,
  model routing, error injection) are new responders, not harness changes. The seams
  take their client on the `Arm`: tests pass a `ScriptedClient` through `Arm::new`
  into the generic entry points (`run_phase`, `consult_with`, `consult_session_turn`),
  while the public `consult`/`oneshot` run on arms the server resolves with
  `Arm::from_slot` — the single live construction point that wraps the real rig client.
  The scripted model's `Response` is a bare `serde_json::Value`, so a responder can hand
  back any provider's *raw* payload shape (`with_raw`) — that's what drives
  `completion_watch` offline.
- **Seeing what the provider said.** rig's agent hook is medium-neutral (content, usage,
  no `raw_response`), so `finish_reason` never reaches kaibo through it. `Watched`
  (`src/completion_watch.rs`) wraps the *model* instead — `traced`'s sibling, a
  transparent `CompletionModel` — recording each turn into a per-call `CompletionLog`
  that `run_phase_logged` hands back, tool-loop turns included, and the last turn's
  reason onto the `run_phase` span. Provider-agnostic by construction: the reason is read
  out of the serialized raw response under the spellings providers ship, so an unknown
  shape degrades to `None` instead of needing a new match arm. The single-shot phases
  (`oneshot`, `deliberate` direct) skip the agent entirely via `Arm::complete` — one
  request, built with rig's own `CompletionRequestBuilder` so both paths move together on
  a rig bump.
- **`docs/issues.md` is the live tracker** — open work only, kept cheap to skim
  before new work. Delete entries when they ship; don't mark them done.
- **`docs/devlog.md`** is a durable narrative from the agent's perspective — write
  your story there.
- **`kaish-kernel` is a published crates.io dep** (pinned in `Cargo.toml`), still
  under active development upstream. A version bump can change its API — when you
  bump, adapt kaibo to the new shape, don't pin around it. (If you're co-developing
  kaish locally, a `[patch.crates-io]` to `../kaish/crates/kaish-kernel` is the way
  — keep it out of committed `Cargo.toml`.) `kaish-mcp` is a useful reference
  sibling, not a dependency.
- **Provider model ids drift.** Built-in defaults (`config.rs::default_models`, keyed
  by `ProviderKind`) seed the built-in casts; rig's bundled model consts are often
  retired. Cross-check the pal configs. Per-cast model overrides live in the XDG
  `config.toml`.

## Build & release

kaibo ships as a single static-ish binary per platform, built by
`.github/workflows/release.yml` on a `v*` tag (also `workflow_dispatch` to smoke the
matrix). This is feasible *because* the TLS invariant above keeps the tree free of
cmake/autotools C (no aws-lc, no OpenSSL) — ring's small C/asm compiles with zig or
the platform cc alone.

- **Linux → fully static** via `x86_64`/`aarch64-unknown-linux-musl`, built with
  `cargo zigbuild` (zig is the cross C compiler/linker for ring's small C/asm). The
  result is `statically linked` / `not a dynamic executable` — runs on any distro.
- **macOS** isn't truly static (Apple forbids static libSystem) but is self-contained
  — a plain `cargo build --release` per arch, depending only on always-present system
  libs.
- **Windows** statically links the CRT via `+crt-static` in `.cargo/config.toml`, so
  it's one self-contained `.exe` (no VCRedist; rustls/ring means no OpenSSL DLL).

Local musl repro (no system zig needed): `pip install ziglang` in a venv,
`cargo install cargo-zigbuild`, then `cargo zigbuild --release --target
x86_64-unknown-linux-musl`. Verify the boundary with `cargo tree -i aws-lc-rs`
(empty) and `ldd target/.../kaibo` (`not a dynamic executable`).

## Writing for models

Every prompt, preamble, tool description, help line, and error string is text some model
reads. When we touch one, we **re-read the whole block and judge it whole** — not the diff
by itself.

**kaibo adopts kaish's `docs/style.md`** — that guide names kaibo as an adopter, and it is
the source for *how* our prose reads: a small vocabulary, one word per concept, exact
numbers, the constraint stated before any qualifier, and one clause of reason after a rule.
Read it before you write help text or operator docs. It sorts files by *weight* — how
strictly its rules apply to that file — and kaibo's files sort like this.

| Weight | kaibo files |
|---|---|
| Full | every tool `description` and schema param doc in `server.rs`; every clap `about`, arg doc, and `--help` line in `cli.rs`; **every error, refusal, and diagnostic string a tool or command returns** |
| Partial — keep the terms and the published/internal boundary; relax the rest | preambles and the kaish cheatsheet (`consult/prompts.rs`, `kaish_syntax.rs`), and `///` on `pub` items |
| Terms only, one line per bullet | `CHANGELOG.md` |
| Terms only | `README.md`, `docs/*.md` |
| Exempt | `docs/devlog.md` — it tells the story from one person's view, and that needs a voice |

**Error strings carry the Full weight above — every rule in the guide applies — and they
are the most valuable prose we own.**
A model reads a refusal more often than it reads a description, and it reads one at the
moment it is blocked — so a refusal changes what the model does next more reliably than any
other text we write. Name three things: what was refused, why, and what to do instead.
`SaveError` in `src/artifact.rs` and `inert_explorer_args` in `src/server/dossier.rs` are
the two to copy.

**Some text kaibo writes reaches a model; the rest never does.** *Published* means kaibo
sends it to a model: a tool `description`, a schema param doc, a clap arg doc, a help topic, an
error string. Everything else is internal — a `///` on a private item, a `//` line, a
module doc-comment — and internal is where implementation detail belongs. The test for a
published sentence is not whether it names an internal detail, but whether the reader needs
that detail to predict what kaibo does. `src/server/dossier.rs` holds its whole design
argument in the module doc and publishes none of it.

**Rulings.** Each of these answers a question the first audit of this guidance could not
settle from the guide alone (2026-08-07, `src/cli.rs`).

- **A subcommand's one-line summary is imperative**, like an example label: it sits beside
  the command name and says what running it does. "Ask one question and exit", not "A
  one-shot question".
- **The 2048-character budget is `server.rs`'s alone.** It is a host truncation of MCP
  instructions and tool descriptions. `--help` has no such limit, so a `cli.rs` line that
  needs a second sentence to state a default or a consequence gets one.
- **Backticks are the only markdown in a clap doc comment.** clap prints the rest
  literally, so `**bold**` reaches the reader as asterisks.
- **A refusal fixable only in config.toml names the exact key, then `kaibo example-config`
  for the shape.** The CLI caller is a person or an agent with file access; both can act on
  a key name, and neither can act on "configure it properly". Name `kaibo configure` only
  when the fix is multi-step setup rather than one key.
- **`family` is a kaibo term** — a model lineage from one vendor, as in "a model outside
  your own family". About forty uses, one sense. Use it; do not paraphrase it.

Three audiences are optimized differently:

- **Client-facing text** — the MCP server instructions and each tool's `description`
  in `server.rs`, read by the *calling* agent. Density is existential here because it's
  *resident* cost: an unused tool still bills the user every session. Hard numbers
  (measured 2026-07-01): Claude Code truncates the instructions **and each tool
  description at 2048 characters** (per server, hardcoded, silent). Claude Desktop
  never shows instructions to the model at all, so **every description stands alone** —
  a model that read nothing else still picks the right tool. In deferral hosts (Claude
  Code, Codex) tool schemas default to names-only *and* the instructions double as the
  tool-search **retrieval index** — the opening lines decide whether our tools get
  *found* (front-load the words a working agent would search for: "codebase", "review",
  "second opinion", "read-only", "batch"); pin a front-door tool resident with
  `_meta["anthropic/alwaysLoad"]` (we pin `consult`). So: the first 2048 characters are
  the whole resident pitch. Decisions above the line (what kaibo is, the casts, scope,
  that an async lane exists); execution detail lives in schemas and resources *named
  from above the line*. The `instructions_*` budget tests in `kaish_syntax.rs` enforce
  the ceiling — a new clause must displace an old one. Terse, concrete, no clause that
  doesn't pay rent.
- **Agent-facing text** — preambles, the kaish cheatsheet, the casts' answering
  instructions, read by the commercial models *we* drive, with windows in the hundreds
  of thousands to millions of tokens. Here verbosity is *licensed where it shapes
  behavior*: say it a few ways, frame positively, be explicit (see **Driving the
  models**). Verbose to install behavior, never verbose by default — plain is not terse:
  keep the repetition that installs an obligation, drop the decoration around it. Why
  "Subset, not slang" binds hardest here: most of our synths are not English-first
  (DeepSeek, GLM, Qwen, Kimi) and the small local models already fixate on odd phrasing,
  so figurative English is a comprehension tax charged to exactly the models we most need
  to work well. `built_in_preambles_are_written_without_em_dash_clause_chains` holds the
  mechanical half.
- **Operator-facing docs** — `docs/config.example.toml`, `docs/config.md`, `README.md`.
  The first two are **embedded in the binary** (`include_str!`) and served as
  `kaibo://config/example` and `kaibo://config/guide`, so a model reads them as often as
  a person does. They are shipped product, not repo notes: technical reference, not a
  record of how we decided. State the rule, name the default, show the shape, and keep the
  rationale to the clause after the dash — the argument behind a decision goes in the
  devlog, and the story of the change goes in the pull request. Split the two files by
  job: the **template** says what to type (a knob, its default, one line on what it does,
  a pointer for the rest); the **guide** explains semantics and interactions. Detail that
  isn't a knob belongs in the guide — the template is read start to finish by whoever is
  configuring kaibo, so every line there is a line they pay for.

## Driving the models

How kaibo talks to LLMs — project defaults, made local so any agent here inherits them.

- **Thinking ON by default**, every model that supports it, both phases (Anthropic
  `thinking`, Gemini `thinkingConfig`, DeepSeek reasoners; in rig via
  `AgentBuilder::additional_params`). The depth is worth the latency/tokens — the
  provider probe showed thinking-capable answers materially deeper than thin ones.
- **Request shaping is model-aware, not just provider-aware** (`ModelShape` in
  `consult.rs`). The thinking block fits the *model*: Anthropic's adaptive tier (Opus
  4.6+/Sonnet 4.6/Fable 5) takes `{type:"adaptive"}` + `output_config.effort` and
  rejects `budget_tokens`/sampling outright; older Anthropic + Haiku 4.5 take
  enabled-budget; Gemini's 3-line takes `thinkingLevel`, 2.5/3.5 `thinkingBudget`.
  Reasoning depth is **per-role effort** (`explorer_effort`/`synth_effort`, default
  `high`) mapped to each provider's field. Boundaries are empirical — a built-in
  classifier with a `thinking_style` config escape hatch; confirm a new model with a
  live probe, don't guess (`tests/consult.rs` has the `#[ignore]`d Anthropic probes).
- **Large token headroom**, because reasoning eats the *completion* budget. Default
  `max_tokens` generously (16k+, not 4k) for every phase — thinking-on means a thin
  budget starves the answer (a Gemma probe spent all 300 `max_tokens` on
  `reasoning_content` and returned empty `content`, `finish_reason: length`). If one
  provider rejects a large value, cap *that arm*, not the global (older DeepSeek
  reasoners capped low; the V4 hybrids advertise 384K output, so the cap is
  per-model — confirm before assuming). Interaction shape behind this: few
  high-value turns, not long chats — spend the budget on depth per turn.
- **Start the model's narrative — a role, not a capability.** Every preamble opens on
  who the model *is* on kaibo's team ("You are the synthesis agent…", "You are the
  explorer…"), in the vocabulary the code already uses for the slots
  (`ModelRole::Synth`/`Explorer`) — an identity is something a model acts *from*, where
  "a capable model" only described it. Obligations ride that identity instead of
  trailing a rule list: the synth's final turn *is* the answer, the explorer's last turn
  *is* the report. And **no length estimates, word counts, or size targets, anywhere** —
  a size cue is a *stopping* cue, and the failure we actually measured is a driver
  stopping too early (a DeepSeek consult ended a turn at 16 of 200 with 14 output
  tokens, no text, a `grep` as its last act — reported as success). Order and obligation
  are what work: "lead with the conclusion … then let your reasoning build under it".
- **Positive prompt framing.** In preambles, tool descriptions, and cheatsheets,
  reinforce the behavior we *want* resident in the weights — say it a few ways —
  rather than prohibiting what we don't: "ground every claim, cite the `file:line`"
  over "never invent citations", and treat naming the edge of the evidence as a
  normal grounded move. Blanket "never X" can light up the very pathway it names and
  make weaker/local models (Gemma especially) fixate or loop. Lead the kaish
  cheatsheet with the good idioms (`cat -n`, `grep -rn`, numbered spans — they produce
  the accurate `file:line`s we reward), not a flat builtin list.
- **Trust grounded evidence; steer toward acquisition, not verification.** When a
  phase is handed context (an explorer report, a prior turn, pasted source), frame
  a grounded `file:line` as *trusted* — the explorer read the real span and is
  rewarded for accurate cites, so a capable synth re-deriving it just burns the
  turn budget the cheap-explorer → capable-synth split exists to save. The behavior
  to install is *get more when the context isn't enough* (an unquoted span, a whole
  file or large chunk for the full picture, a detail left open, anything the
  question reaches past) — not *re-confirm what's likely right*. This is also the
  better anti-bias posture: bias lives in a report's gaps and framing, not its cited
  facts, so the cure is investigation that *extends* the context, not re-checking it.
  Keep a "the code is the only ground truth" tiebreaker for genuine conflicts (it
  fires only when one is noticed; it doesn't send the model hunting). See the context
  framing in `consult.rs` (`consult_preamble`, `consult_user_prompt`) — the seam that
  absorbed the old standalone `synthesize`.

## Commit style

Commit and pull request bodies should usually summarize the decisions behind the
change, **drawn from the conversation with the user**. Commit messages briefly explain
what happened as context for the more important task of explaining the decisions we
made.

Credit the loop in trailers when it materially shaped the change. Use `Co-authored-by`
for an agent that wrote or substantially rewrote the patch and `Reviewed-by` for a
model or tool review that changed confidence or content (for example, a kaibo cast
review). Prefer concrete names and stable identities when the host supplies them; if it
doesn't, name the model/tool plainly in the PR body instead of inventing an email.

Stage deliberately. Do not use `git add -A`, `git add .`, or broad globs when preparing
commits; stage only the files intentionally changed for the current task. Check
`git status --short` before committing, and leave unrelated/untracked user files alone.

## Pull requests & the changelog

Every change lands through a pull request — `main` is never committed to directly.
This is for **transparency**: kaibo is used by other people's agents, and the PR
trail is how a user can see what we're up to — what changed, why, and what review it
got — without reading the diff. So the discipline isn't about code risk; it holds
even for a one-line doc fix.

- **Branch → PR → review → merge, always.** Every change starts on a branch and goes
  up as a PR. There is no "small enough to push to `main`" carve-out — the point is
  the visible trail, and a trivial change is trivial to review and merge. Dogfood the
  review: run a **cross-family** pass over the diff — a different model lineage than
  wrote it (`/code-review`, or kaibo's own `consult`/`oneshot` aimed at the change) —
  before merge. Scale the review to the change (a doc fix is a glance; a sandbox or
  TLS change is a hard look), but don't skip the PR.
- **Stacked PRs — temporary guidance (GitHub public preview, 2026-07).** When one
  change builds on another unmerged change, prefer a native GitHub stack over sibling
  PRs racing to `main` — the stack resolves their overlap once, before any merge.
  Primer: the CLI is `gh stack` (`gh extension install github/gh-stack`).
  `gh stack link <bottom> <top>…` retrofits open PRs or branches into a stack,
  retargeting each PR's base onto the one below; `gh stack init`/`add`/`submit` is the
  local-first path. `gh stack merge` lands a layer plus every unmerged layer below it
  in one all-or-nothing operation, and layers above a merged one rebase and retarget
  automatically — a linked stack cannot strand a PR on an already-landed base branch.
  Layers touching the same files still need a real rebase on the branch: additive
  docs files (changelog, tracker, devlog) resolve as the union in stack order, code
  conflicts resolve on their logic, then re-run the gates and push
  `--force-with-lease` (`gh stack submit` when using local tracking). An
  *unstacked* PR keeps the manual base check before merge:
  `gh pr view <N> --json baseRefName`. Delete this guidance when the harness injects
  stack awareness or the models carry the feature natively.
- **Every user-facing change updates `CHANGELOG.md`** under the top *unreleased*
  section, in the Keep a Changelog buckets (Added / Changed / Fixed / Security / …).
  Write what a *user* notices, not the file diff. Internal-only refactors need no entry —
  the git log is their record (mirrors the `docs/issues.md` "delete when shipped"
  discipline).
- **A changelog bullet is one line: the change, and at most one clause of reason.** The
  full reasoning belongs in the pull request body, which becomes the merge commit, so the
  bullet does not carry it. If a bullet needs three numbers and three reasons, write three
  bullets. **Entries get shorter over time.** When you add an entry, shorten any entry near
  it that is longer than one line, and shorten the whole unreleased section before you
  retitle it for a release. Someone scanning for what changed should not have to read the
  reasoning behind it.
- **Cutting a release.** Bump `version` in `Cargo.toml`, retitle the unreleased
  section to `## [X.Y.Z] — <date>` and open a fresh empty unreleased section above it,
  then tag `vX.Y.Z` — `.github/workflows/release.yml` builds the platform matrix on a
  `v*` tag. Before tagging: confirm the `kaish-kernel` and `turso` pins are current, re-run
  `docs/sandbox-probes.md` and stamp its "Last run" line, and verify `cargo tree -i` is
  empty for `aws-lc-rs` and `mimalloc` and the musl binary is `not a dynamic executable`.
  After the release publishes: run the README "Verify a download" commands against a
  fresh asset (`gh attestation verify`, `cosign verify-blob` with the new tag's
  identity) — the tag-gated publish job signs releases, and signing an operator can't
  verify is theater, so prove it the way a user would. Then run `scripts/bump-tap.sh
  vX.Y.Z` to point the Homebrew tap (`tobert/homebrew-kaibo`) at the new release — it
  reads that release's `.sha256` sidecars and pushes with your own `gh`/git auth, so
  there's no CI secret to rotate. Deliberately manual: releases are human-cut, so this
  is the ritual's last step, not a workflow job.
- **kaish pin.** Currently `kaish-kernel = "0.14.0"`. The `0.13.0 → 0.14.0` bump moved
  **one** call site, and it was a deletion: `kaish_syntax.rs`'s
  `strip_write_side_paragraphs`, which stripped the shared contract's "Overlay mode"
  paragraph before it fronted kaibo's read-only shell. kaish-help #297 made that guidance
  opt-in (`Concept::Overlay`, reached only via `Selector::with_overlay`), so the paragraph
  stopped arriving and the filter had nothing to remove — its own guard assertion is what
  failed and told us. The *property* kept its test (`core_carries_no_write_side_teaching`):
  it now rests on upstream's default plus kaibo never opting in, and it still fails if
  either changes (proven by sabotage — composing `.with_overlay()` turns it red).
  Everything else on the checklist was inert here: `jobs --json` gaining a `path` field and
  lowercase `JobStatus` wire spellings (including the new `Killed`) reach no parse site,
  because model-facing kaish never grows `jobs`; `KernelConfig::agent()` defaulting
  `kill_on_parent_death = true` has no children to signal with `subprocess` off;
  `Kernel::shutdown` taking `&self` matters only to a caller, and kaibo never calls it;
  `ExecResult.approval` carrying `PendingApproval` is unreachable from a read-only
  embedder. `ExecuteOptions`, `OutputLimitConfig`, and `ToolArgs` were untouched.
  **The one thing this bump changes about the boundary:** kaish mounts an approvals ledger
  at `/v/approvals` in every kernel, unconditionally — a model-readable path kaibo did not
  have before. It is inert here (a fresh kernel per call means its observation log holds
  only that call's own commands, and every mutation is refused), but its read-only-ness
  comes from kaish's `vfs/approvalsfs.rs` chokepoint, **not** from any of kaibo's four
  levers, because `/v/*` is kernel space our backend never sees. Battery F in
  `docs/sandbox-probes.md` is how we keep checking the part of the boundary we don't own.
- **The previous pin, kept because the reasoning pattern is the point.** The
  `0.12.0 → 0.13.0` bump was again API-compatible — **zero** call-site changes, `cargo build`/`clippy --all-targets`/
  full `cargo test` (598 passed) all clean, `cargo tree -i aws-lc-rs` and `-i mimalloc`
  both empty. A large release (six crates bumped, ~270 changelog lines: lexer/parser
  fixes, stricter binary-input handling across several builtins, a browser/wasm target)
  but its **BREAKING (embedders)** entries all land on surface kaibo doesn't touch:
  buffered stdin going `Vec<u8>` instead of `String` (`ExecContext::stdin`/
  `ExecuteOptions::stdin`, `read_stdin_to_string` removed) only bites a caller that
  feeds/reads kaish's own stdin, which kaibo never does (`cli.rs`'s `--attach`/piped-stdin
  handling is unrelated CLI-level plumbing, not kaish's `ExecContext`); `ToolArgs::to_argv()`
  returning `Result` instead of a bare `Vec<String>` only bites a caller that calls it —
  `Blocked::execute` (`sandbox.rs`) receives a `ToolArgs` parameter but never calls
  `.to_argv()` on it; and `output_limit::spill_aware_collect` + its private helpers being
  removed is internal plumbing behind `OutputLimitConfig` (`.agent()`/`.set_limit()`/
  `.with_output_limit()` in `sandbox.rs`), which the kaish changelog states explicitly is
  unaffected. Precedent that a bump *can* move call sites: `0.8.4 → 0.9.0` renamed the
  `mcp()` config constructors to `agent()` in `sandbox.rs`. Keep this current per the
  **Working here** kaish-bump discipline before cutting.
