# AGENTS.md — kaibo (解剖)

Kaibo is a stdio MCP server: an assistant agent **for other agents**. It augments a
calling agent (Claude, etc.) with a team of models, lending two kinds of help —
*consultation* (grounded, cited, read-only answers about a codebase) and
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

**Capabilities** are a distinct, growing tool *class* — not `run_phase` loops:

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

- **Read-only is the product.** Enforced in `src/sandbox.rs` by four levers: a
  read-only mount, `MemoryFs` at `/`, external commands disabled, and a `DENYLIST`
  of builtins that reach real state *directly* and bypass the mount (git, touch,
  spawn, exec, kill, mktemp — see the module doc-comment). Any change here keeps
  `tests/sandbox.rs` green and adds a test that can fail. Read-*scope* is also
  bounded: every call's path must canonicalize (symlinks, `..` resolved) into the
  allowed set (`--root` / `--allow-path`, launch cwd when unset), enforced in
  `server.rs::resolve_root` with tests in `tests/containment.rs`.
- **stdio only.** kaibo can read a filesystem, so it must never bind a socket.
- **kaish is `!Send`.** The kernel runs on a dedicated thread behind `KaishWorker`;
  rig tools require `Send` futures. Don't hold the kernel across an `.await`.

## Working here

- **TDD.** Tests that can and will fail. The sandbox boundary gets failing-first
  tests — and we prove they have teeth (empty the `DENYLIST`, watch them fail).
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
- **`kaish-kernel` is a path dep** (`../kaish/crates/kaish-kernel`), under active
  development. It will break kaibo's build transiently — adapt to its new API,
  don't pin around it. `kaish-mcp` is a useful reference sibling, not a dependency.
- **Provider model ids drift.** Built-in defaults seed the profile registry in
  `config.rs::default_models`; rig's bundled model consts are often retired.
  Cross-check the pal configs. Per-profile overrides live in the XDG `config.toml`.

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
