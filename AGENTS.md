# AGENTS.md — kaibo (解剖)

Kaibo is a stdio MCP server that provides an assistant agent **for other agents**.
It augments a calling agent (Claude, etc.) with a team of models, lending one kind of
help — *consultation*: grounded, cited, read-only answers about a codebase. The team
*perceives* what fuses into its reasoning (image input today — `view_image` and image
attachments on the model-driven tools; more modalities as the models gain them), but
kaibo produces **no output artifacts** — it reasons over code, it doesn't render or
emit. (If it ever needs to *record* something, that's a specific mediated tool, not a
general write path; see the read-only invariant.)

**Consultation: one primitive, three tools.** The primitive is `run_phase`
(`consult.rs`): a model + preamble + an *injected toolset*, run as a bounded tool
loop. Each consultation tool is that loop wearing different clothes:

- **`consult`** — a capable model with `{run_kaish, explore′}` + optional caller
  `context`: it reads precise spans directly and delegates broad sweeps to a cheap
  explorer sub-agent, then answers. No rigid explorer→synth hand-off; the model
  chooses. Supplied context is trusted starting evidence — it investigates for
  *more*, not to re-verify. The `explore′` sweep (`report_preamble`) lives *inside*
  this loop now; it is not a standalone tool.
- **`oneshot`** — a capable model with **no tools**: the caller owns the context, so
  it's one upstream request, prompt in / answer out, no codebase access. The thin
  counterpart to `consult` — and the door to a model outside the caller's family.
- **`run_kaish`** — drive the read-only kaish shell directly, no model in the loop.

Both model-driven tools name their cast + answering model(s) in a provenance footer
(`with_provenance` in `server.rs`), so a cross-model study sees which model answered.

Each tool is independently gated by a `--no-<tool>` flag (all on by default; the
all-off server is refused at startup). Multi-provider over `rig-core`: a
**`ProviderKind`** is the wire protocol (keyed Anthropic / DeepSeek / Gemini, plus
**`openai`** for any OpenAI-compatible endpoint). A **`[backends.<name>]`**
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
  plus the **single blessed `create_dir_all`** that `tests/no_write_path.rs` carves out —
  every other `std::fs` mutation in `src/` still fails that guard. That store is the one
  deliberate write surface; anything else that must *record* or *emit* is a specific
  mediated tool, never a general filesystem escape hatch or a loosening of the four
  levers. Read-*scope* is also bounded: every call's path must canonicalize (symlinks,
  `..` resolved) into the allowed set (`--root` / `--allow-path`, launch cwd when unset).
  Enforced in `server.rs::resolve_root`, with tests in `tests/containment.rs`.
- **Operator surface vs. the model team — do not blur.** The trust model the whole
  design rests on: the model-facing shell cannot modify the world (unconditional, above),
  and kaibo acts only on *kaibo's own* things (the XDG state dir today) through
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
  tests — and we prove they have teeth (mount the project writable with
  `LocalFs::new` instead of `read_only`, watch the write-denial tests fail).
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

Every prompt, preamble, tool description, and cheatsheet is text some model reads.
When we touch one, we **re-read the whole block and judge it holistically** — not the
diff in isolation. The ratchet is *compression* (改善): over time a block should carry
more impact per token and avoid accreting clauses.

Two audiences are optimized differently:

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
  models**). Verbose to install behavior, never verbose by default.

## Driving the models

How kaibo talks to LLMs — Amy's defaults, made local so any agent here inherits them.

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
- **Every user-facing change updates `CHANGELOG.md`** under the top *unreleased*
  section, in the Keep a Changelog buckets (Added / Changed / Fixed / Security / …).
  Same "why not what" ethos as commits: write what a *user* notices, not the file
  diff. Internal-only refactors need no entry — the git log is their record (mirrors
  the `docs/issues.md` "delete when shipped" discipline).
- **Cutting a release.** Bump `version` in `Cargo.toml`, retitle the unreleased
  section to `## [X.Y.Z] — <date>` and open a fresh empty unreleased section above it,
  then tag `vX.Y.Z` — `.github/workflows/release.yml` builds the platform matrix on a
  `v*` tag. Before tagging: confirm the `kaish-kernel` and `turso` pins are current, re-run
  `docs/sandbox-probes.md` and stamp its "Last run" line, and verify `cargo tree -i` is
  empty for `aws-lc-rs` and `mimalloc` and the musl binary is `not a dynamic executable`.
  After the release publishes: run the README "Verify a download" commands against a
  fresh asset (`gh attestation verify`, `cosign verify-blob` with the new tag's
  identity) — the tag-gated publish job signs releases, and signing an operator can't
  verify is theater, so prove it the way a user would.
- **kaish pin.** Currently `kaish-kernel = "0.12.0"`. The `0.11.0 → 0.12.0` bump was
  again API-compatible — **zero** call-site changes, `cargo build`/`clippy`/`cargo tree -i
  aws-lc-rs` all clean. The changelog's **BREAKING (embedders)** entries all land on
  compiled-out or unused surface: `ExecContext.tool_schemas` moving `Vec<ToolSchema>` →
  `Arc<[ToolSchema]>` only bites direct field assignment (kaibo never constructs
  `ExecContext`), and `JobStatus`/`JobInfo`/`ToolResult` going `#[non_exhaustive]` (plus
  the new `JobStatus::Latched` variant and `LatchRequest.job_id`) only bites background
  jobs and confirmation latches — kaibo compiles with `subprocess`/`host`/`os-integration`
  off, so `bg`/`fg`/`jobs`/`kill`/`spawn` and the whole latch surface don't exist in this
  binary. Two things *did* change kaibo's own text and code, both improvements landing
  for free from upstream fixes: `grep -r PATTERN FILE` (GH #105) now searches the file
  instead of silently finding nothing, so `KAISH_SANDBOX_ADDENDUM`'s single-file caveat
  (`kaish_syntax.rs`) was stale and dropped, and the matching `docs/issues.md` tracker
  entry (already shipped) was deleted. And the new `MAX_RECURSION_DEPTH`
  (48)/`RECOMMENDED_STACK_SIZE` (12 MiB) matched pair — kaish's own guard against a
  runaway `$(...)`/shell-function/`.kai`-source recursion overflowing the native stack —
  is now what sizes `KaishWorker`'s dedicated thread in `sandbox.rs`, replacing a
  hand-picked 16 MiB literal with the constant kaish itself documents for embedders
  (`docs/EMBEDDING.md` in the kaish tree); live-probed with `f() { f; }; f`, which now
  fails loudly (`"maximum recursion depth (48) exceeded"`) instead of risking a SIGSEGV.
  Offline suite green (531 tests), boundary tests still have teeth. Precedent that a bump
  *can* move call sites: `0.8.4 → 0.9.0` renamed the `mcp()` config constructors to
  `agent()` in `sandbox.rs`. Keep this current per the **Working here** kaish-bump
  discipline before cutting.
