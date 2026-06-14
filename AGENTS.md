# AGENTS.md — kaibo (解剖)

Kaibo is a stdio MCP server that provides assistant agent **for other agents**.
It augments a calling agent (Claude, etc.) with a team of models, lending two
kinds of help — *consultation* (grounded, cited, read-only answers about a codebase) and
*capabilities* (things the team can *do* and hand back as artifacts; image generation
today, more as `rig` grows coverage).

**Consultation: one primitive, four tools.** The primitive is `run_phase`
(`consult.rs`): a model + preamble + an *injected toolset*, run as a bounded tool
loop. Each consultation tool is that loop wearing different clothes:

- **`consult`** — a capable model with `{run_kaish, explore′}`: it reads precise
  spans directly and delegates broad sweeps to a cheap explorer sub-agent, then
  answers. No rigid explorer→synth hand-off; the model chooses.
- **`explore`** — a cheap model with `{run_kaish}` → a curated report (the seam).
- **`synthesize`** — a capable model with `{run_kaish}` + optional caller `context`
  → an answer (investigates directly when context is thin).
- **`run_kaish`** — drive the read-only kaish shell directly, no model in the loop.

**Capabilities** are a distinct, growing tool *class* — not `run_phase` loops. The
direction is the tell: consultation and perception (`view_image`) run images and
context *into* kaibo's own models so they can reason; a capability runs a model and
hands the **artifact back to the calling agent** — kaibo is the producer, the caller
is the consumer.

- **`generate_image`** — prompt → image, returned inline as MCP `Content::image`
  (`generate_image.rs`, `image_gen.rs`). A single provider call behind the `ImageGen`
  seam; no shell, no model loop. Resolves the cast's `image` slot, openai-kind only
  (rig 0.38 has no image path for the keyed protocols — refused honestly otherwise).

Each tool is independently gated by a `--no-<tool>` flag (all on by default; the
all-off server is refused at startup). Multi-provider over `rig-core`: a
**`ProviderKind`** is the wire protocol (keyed Anthropic / DeepSeek / Gemini, plus
**`openai`** for any OpenAI-compatible endpoint). A **`Profile`** (`config.rs`) is a
*named instance* of a kind with its own base URL, key source, and models — so two
`openai` profiles (hosted GPT and a local Gemma/llama.cpp server, say) can be live
at once, each selected by the `provider` arg. Profiles come from a built-in registry
merged under an XDG `config.toml`, `KAIBO_*` env, then CLI flags (precedence:
per-call > CLI > env > file > built-in); a missing config file is a non-error.
See `docs/config.md`. kaibo never modifies the project and cannot run external
commands.

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
  a test that can fail. Read-*scope* is also
  bounded: every call's path must canonicalize (symlinks, `..` resolved) into the
  allowed set (`--root` / `--allow-path`, launch cwd when unset), enforced in
  `server.rs::resolve_root` with tests in `tests/containment.rs`.
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
  model routing, error injection) are new responders, not harness changes. Inject via
  the generic seams (`run_phase`, `consult_with`, `consult_session_turn`); the public
  `consult`/`explore`/`synthesize` build the real client behind `with_provider_client!`.
- **`docs/issues.md` is the live tracker.** Skim it before new work. Delete
  entries when they ship — don't mark them done; git history is the record.
- **`kaish-kernel` is a published crates.io dep** (pinned in `Cargo.toml`), still
  under active development upstream. A version bump can change its API — when you
  bump, adapt kaibo to the new shape, don't pin around it. (If you're co-developing
  kaish locally, a `[patch.crates-io]` to `../kaish/crates/kaish-kernel` is the way
  — keep it out of committed `Cargo.toml`.) `kaish-mcp` is a useful reference
  sibling, not a dependency.
- **Provider model ids drift.** Built-in defaults seed the profile registry in
  `config.rs::default_models`; rig's bundled model consts are often retired.
  Cross-check the pal configs. Per-profile overrides live in the XDG `config.toml`.

## Build & release

kaibo ships as a single static-ish binary per platform, built by
`.github/workflows/release.yml` on a `v*` tag (also `workflow_dispatch` to smoke the
matrix). This is feasible *because* the TLS invariant above keeps the tree C-free:
no aws-lc, no OpenSSL.

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
  cheatsheet with the good idioms (`cat -n`, `rg -n`, numbered spans — they produce
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
  fires only when one is noticed; it doesn't send the model hunting). See the synth
  prompts in `consult.rs` (`synthesize_preamble`, `synthesize_user_prompt`).

## Commit style

Commits explain **why, not what** — the diff already shows what changed. Write the
body as a short summary of the *decisions* behind the change and their rationale,
drawn from the working conversation: what we chose, what we rejected, and why (when
a why was stated). A few sentences of reasoning beat a bullet list of files.

- **Subject:** imperative, the decision or outcome — not "update sandbox.rs".
- **Body:** the reasoning and tradeoffs. Cite a decision's source when it matters.
- Don't narrate the code; point to `docs/issues.md` for follow-ups.
- End every message with a `Co-Authored-By:` trailer crediting the model that
  actually did the work (might not be a Claude — we are a community here), e.g.
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

Example:

```
Sandbox the explorer behind a read-only kaish

The explorer must read the project but never mutate it. Chose kaish's read-only
VFS mount plus a denylist over a hand-rolled file API: the mount makes "read-only"
structural rather than honor-system. We confirmed git/touch bypass the mount
(unblocked, `git init` returned 0 and made a real .git), so those are shadow-
blocked at the registry too. Tests prove the boundary and that the blocks have
teeth.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
```

## Pull requests & the changelog

From **0.2.0** on, kaibo maintains a changelog and lands changes through pull
requests — `main` is protected by convention, not a scratchpad.

- **Branch → PR → review → merge.** Non-trivial work lands on a branch and goes up
  as a PR, not direct-to-`main`. Dogfood the review: run a **cross-family** pass over
  the diff — a different model lineage than wrote it (`/code-review`, or kaibo's own
  `consult`/`synthesize` aimed at the change) — before merge. A typo or a one-line doc
  fix can still go straight to `main`; this is judgment, not ceremony.
- **Every user-facing change updates `CHANGELOG.md`** under the top *unreleased*
  section, in the Keep a Changelog buckets (Added / Changed / Fixed / Security / …).
  Same "why not what" ethos as commits: write what a *user* notices, not the file
  diff. Internal-only refactors need no entry — the git log is their record (mirrors
  the `docs/issues.md` "delete when shipped" discipline).
- **Cutting a release.** Bump `version` in `Cargo.toml`, retitle the unreleased
  section to `## [X.Y.Z] — <date>` and open a fresh empty unreleased section above it,
  then tag `vX.Y.Z` — `.github/workflows/release.yml` builds the platform matrix on a
  `v*` tag. Before tagging: confirm the `kaish-kernel` pin is current (next bullet),
  re-run `docs/sandbox-probes.md` and stamp its "Last run" line, and verify
  `cargo tree -i aws-lc-rs` is empty and the musl binary is `not a dynamic executable`.
- **Release gate — kaish bump pending.** A bug surfaced upstream right after kaish
  `0.8.3` (the current `Cargo.toml` pin); 0.2.0 waits on the fixed release. Bump the
  pin, adapt to any API change rather than pinning around it (per **Working here**),
  and re-green the offline suite before cutting. Tracked in `docs/issues.md`.
