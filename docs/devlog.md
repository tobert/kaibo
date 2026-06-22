# kaibo — Devlog

解剖（かいぼう）'s shipped-work record: the *why* behind landed changes, newest
first. This is the curated narrative git can't carry — what we chose, what we
rejected, what a live probe proved, how the shipped surface drifted from the plan.

It's the other half of the `docs/issues.md` discipline: open work lives there and is
*deleted* when it ships; the reasoning behind that ship lands *here* as a dated entry.
Skim `issues.md` before new work; skim this when you need to know why something is the
way it is and the commit log is too thin.

Append-only by intent — entries don't get edited away, the log grows. One `##` heading
per ship date; multiple ships on a date get sub-bullets.

---

## 2026-06-22 — batch: offline max-effort fan-out (Anthropic first)

Shipped the batch tool class — `batch_submit`/`batch_get`/`batch_cancel`/`batch_list`,
the **offline, async sibling of `oneshot`**. The shape was the whole debate. Started
from "batch mode as a param on `oneshot`" and rejected it: batch is a *different class*,
not a flag. It's toolless **by construction** — provider batch APIs are offline and
can't drive a tool loop — so it's built on the `oneshot` *shape* (a capable model
answering from what it was handed), never `run_phase`. Separate verbs rather than one
tool with modes, because submit→poll→cancel→list are genuinely distinct and "kill a fat
top-tier batch" is a real button to want.

Designed fresh for kaibo, not ported from gpal/cpal — those were a reference for what's
*possible*, not a template. Decisions worth recording:

- **Max the knobs by default.** Batch floors `max_tokens` and forces thinking at
  `BATCH_EFFORT` regardless of how the cast's synth slot was tuned for interactive use.
  The motivating case (Amy's): Gemini Pro is near-unusable interactively, so casts synth
  on Flash — but batch is exactly where you reach for Pro, and the latency that makes
  max-thinking painful synchronously is *free* once you've accepted "come back later."
  `BATCH_EFFORT` is threaded *explicitly* through `ModelShape::to_params` rather than
  via the `consult::thinking_params` helper — a first cut reused that helper, but it
  would have silently baked in `consult`'s interactive `DEFAULT_EFFORT`; a cross-family
  review caught the dead-constant trap, so batch now owns its effort independently
  (`max_tokens` is a *floor*, never undercutting a richer slot).
- **No state, by design.** kaibo holds nothing on disk, and we kept it that way: the
  returned handle is `backend/provider-id`, the *whole* address. Poll/cancel/list
  rebuild a fresh client from the backend and re-address the provider's own batch id, so
  they survive a server restart. The split trusts a backend name to carry no `/` — so we
  *enforced* that at config load (`config.rs`) rather than leaving it a convention the
  slot-ref and batch-handle parsers silently bank on.
- **A preamble of its own.** Batch shares `oneshot`'s toolless shape but not its words.
  A cross-family review (Opus, run *through* `batch_submit` itself — dogfooding the
  tool to critique its own design) flagged that `oneshot`'s "name what you'd need rather
  than guessing" is right *synchronously* (a gap invites a next turn) but wrong offline
  (there is no next turn — stopping at "I'd need X" burns the caller's one shot). So
  `batch_preamble` (overridable via `[prompts].batch`) tells the model it gets one
  complete, self-contained response, to spend the forced budget on depth, and to
  *state an assumption and answer under it* rather than stall — and drops the negative
  "guessing" framing for the positive form the CLAUDE.md rule wants.
- **`batch_list` closes the orphan gap.** No state means a *lost* handle is a batch that
  keeps billing with no way back to it — the review's sharpest finding. `batch_list`
  reads the provider's own batch list (the source of truth kaibo doesn't keep): newest
  first, each entry a ready-to-use handle with status and progress, defaulting across
  every Anthropic backend (you may not recall which ran it) or scoped to one.
  Per-backend failures and a truncated page are surfaced, never hidden. Live-verified
  against 14 real historical batches — recovered every one by handle.

**Anthropic first, deliberately.** Message Batches is inline (requests in one POST) and
the wire shape is one we're confident about; Gemini (Amy's real want) and OpenAI
(file-based) follow as their own PRs once a live probe confirms each shape — the
"confirm, don't guess" discipline. A non-Anthropic cast is refused with a clear message
rather than silently no-oping, the same honest-absence posture as the `ImageGen` seam.
`batch_submit` is many-prompts/one-cast for now; the one-question/many-casts panel is N
provider batches under a composite handle, deferred (`docs/issues.md`).

The whole seam is offline-tested: pure request-shaping/response-parsing/render fns plus
a `ScriptedBatch` driving submit→pending→done and a seeded list with no network,
mirroring `ScriptedImageGen`. The live wire is proven by trying it (the dogfooded review
and the `batch_list` recovery run), not by a unit test that can't reach the provider.

## 2026-06-18 — kaish-kernel 0.9.0

Bumped the published dep `0.8.4 → 0.9.0`. One API break carried through: the `mcp()`
config constructors (`IgnoreConfig` / `OutputLimitConfig` / `KernelConfig`) were
renamed to `agent()` — same presets, embedder-facing name only. Adapted the three call
sites in `sandbox.rs`. Offline suite green, boundary tests still have teeth. Per the
kaish-bump discipline: adapt kaibo to the new shape, don't pin around it.

## 2026-06-17 — follow git worktrees of an already-allowed repo

A worktree of an allowed repo is now reachable without a separate `--allow-path`. The
`[runtime]` config section grew `follow_worktrees` (the knob) and `followed_worktrees`
(the live extra set granted beyond `allowed_paths`, recomputed each read so a worktree
created mid-session shows up). Keeps the containment invariant honest — the grant is
*observed* and rendered, not silently widened.

## 2026-06-16 — consultation surface collapsed to `consult` + `oneshot`

Offline-green. The driver was a cross-model-study finding: agents reach for the
per-model pals over kaibo. The pals win by naming the model in the tool and offering two
shapes (agentic + a thin oneshot); kaibo led with "a codebase" and *four* tools, two of
which (`explore`, `synthesize`) were internal seams leaking onto the public surface.

Fix: `consult` gained an optional `context` seed — absorbing `synthesize`'s
trusted-evidence behavior, with the `explore′` sweep staying internal to its loop. Added
a toolless `oneshot` (prompt in / answer out, no codebase access — the pal-shaped thin
path). Removed `explore` / `synthesize` as public tools, with their `--no-*` flags,
`[prompts]` keys, and config rows. Both model tools now describe themselves as the door
to a model *outside the caller's family*, name the casts up front, and append a
provenance footer (cast + answering models) so a study sees which model answered without
opening `kaibo://config`; `consult` also steers "say what you did, don't paste a diff."

Tests ported to the new seams: `view_image` now exercises the `consult` driver where it
rides; the `synthesize` prompt tests became consult-with-context tests; new tests pin
`oneshot`'s empty toolset and the provenance footer. Landed via the
`client-instructions-say-what-you-did` PR.

## 2026-06-13 — `generate_image`: kaibo's first capability

Live-verified. This is kaibo's first *capability* (vs. consultation) — the artifact
goes back to the caller, kaibo doesn't reason over it. `generate_image`
(`generate_image.rs` + a `server.rs` handler) is a dedicated MCP tool, **not** a
`run_phase` costume and **not** a kaish builtin: it resolves the cast's `image` slot into
an `ImageGen` (`image_gen.rs`), calls rig's openai `ImageGenerationModel`, sniffs the
MIME (shared `view_image::sniff_mime`), and returns the bytes inline as `Content::image`
+ a caption.

Openai-kind only — rig 0.38 has no image path for the keyed
Anthropic/Gemini/DeepSeek protocols, so a non-openai `image` slot is refused loudly (the
same honesty as parked TTS); enabled by rig-core's `image` feature (zero extra deps).
Gated `--no-generate-image` / `KAIBO_NO_GENERATE_IMAGE`, all-off still refused at
startup. Inline-only with a size cap (`DEFAULT_MAX_IMAGE_BYTES`); over-cap is a loud
error, never a silent drop.

Offline tests cover parse/sniff/cap/content + the openai-only resolver gate + tool
gating; the **live probe** (`tests/image_gen_live.rs`, `#[ignore]`) generated a real
569 KB PNG via local lemonade `SD-Turbo-GGUF` over `/v1/images/generations` in ~9s — so
this is live-works, not just offline-green.

**Surface change from the plan:** image gen was scoped as a *kaish builtin* (for shell
composition); we shipped it as a capability tool instead — the basic "agent asks for an
image" path wants a direct call, and the builtin/VFS-composition surface is re-homed
under image2image/media pipelines, deferred. Deferred follow-ons: `--out-dir` +
`ResourceLink` for large artifacts; per-builtin timeout (moot for a direct tool — the
per-backend `request_timeout` governs); the builtin/VFS composition surface; non-openai
image kinds pending rig coverage.

## 2026-06-12 — `view_image` on OpenAI-compatible VLMs

Offline-green — the OpenAI vision-channel fix. An `openai` vision slot now
genuinely *sees*. `view_image` still produces the tool-result image envelope, but on a
transport that can't carry it (the OpenAI wire forbids an image in a `role:tool` message;
rig 400s first), `run_phase` (`consult.rs`) installs a `ViewImageBreakHook` that flags on
`on_tool_result` and terminates on the **next** `on_completion_call` — the turn boundary
where rig's transcript already holds every tool result of the triggering turn, so
co-tool-call orphaning is structurally impossible (verified against rig 0.38.2
`prompt_request/mod.rs:665-672,1081`).

The `PromptCancelled` transcript is rewritten (`rewrite_view_image_history`): each
`view_image` result becomes a text ack and a *separate*, tool-result-free
`Message::User { [Image] }` lands after it (mixed in one turn, rig's openai converter
silently drops the image — the load-bearing S2 result), then the loop resumes via the
`finalize_prompt`-style split with a transcript-derived outer turn budget so a looping
`view_image` can't refresh `max_turns`. Gated on a new see-∧-transport predicate:
`ModelCaps.tool_result_images` (= `transport_supports_tool_result_images(kind)`);
anthropic/gemini keep the tool-result channel untouched.

Offline tests: pure rewrite (separate-message, idempotency, co-tool-call selectivity) +
two driven loop tests. **Caveat that was open at ship:** the scripted mock returns its
answer regardless of wire validity, so a rewrite that left an orphaned `tool_use` passes
offline; only a real openai-compatible VLM (local Qwen-VL) reporting a detail it could
only *see* confirms it — the live probe against a real VLM was load-bearing, not
optional.

## 2026-06-11 — vision-in (`view_image`)

A vision-capable phase reads an image *file* from the workspace and the bytes reach model
context as a rig image part (`src/view_image.rs`). Path-only by decision (debug
screenshots/assets/docs are files already in the tree); no MCP-native/inline input.

Bytes are read through the project VFS via a new `KaishWorker::read_file` (a `Job::Read`
on the worker thread → the *project* `VfsRouter`, retained from
`build_readonly_kernel_and_vfs` because under `with_backend` the kernel's own `vfs()`
carries only `/v/*` scratch), so containment + read-only stay structural and the
script-output cap is bypassed for the deliberate read. Toolset assembly gates
`view_image` on `arm.caps.vision` in all phases; a blind model never sees the tool, so
there's no fail-loud attach path.

Two correctness landmines caught by reading ground truth, not guessing: rig's part key
is camelCase **`mimeType`** (not the `mime_type` an earlier note claimed), and
`Tool::Output` must be a `serde_json::Value` object — rig `serde_json::to_string`s the
output, so a `String` arrives double-encoded and `from_tool_output` treats it as text,
never an image (the offline round-trip test proves the whole chain). Out-of-workspace
paths get an actionable copy-it-in error; MIME by magic-byte sniff; a loud size cap, no
resize dep.

## 2026-06-11 — kaish 0.8.1 bump + scratch `ByteBudget`

The dep moved to published `kaish-kernel = "0.8.1"` (clean, no API breakage), and the
unbounded-`/`-scratch surprise is closed: `[sandbox].scratch_limit_bytes` (env
`KAIBO_SCRATCH_LIMIT_BYTES`, default 64 MB, must be > 0 — no "unbounded" escape) threads
an owned labeled `ByteBudget` onto the scratch `MemoryFs` via `MemoryFs::with_budget` at
`sandbox.rs` construction, so a runaway redirect fails loudly (`StorageFull`) instead of
eating host RAM for the kernel's lifetime. `ByteBudget` rides `kaish_kernel::vfs` — no
direct kaish-vfs dep. Failing-first test in `tests/sandbox.rs` (proven teeth by swapping
in an unbounded mount).

Bonus from the bump: kaish fixed "redirects ignore cwd," so a bare relative `> f` now
resolves against cwd — the sandbox tests target absolute scratch paths accordingly. A
DeepSeek review of the backends/casts config also landed one fix: the table slot form
now trims `backend`/`id` like the string-ref form, so identical intent doesn't hinge on
spelling.

## 2026-06-11 — backends/casts split

`docs/casts.md` stands as the design record. `Profile` dissolved into
`[backends.<name>]` (connections) + `[casts.<name>]` (role → `"backend/model-id"`,
freely cross-backend), calls select casts via the `cast` param, each slot resolves to its
own `Arm` (client + request shape) behind the decided vtable seam. Built-in equivalence
holds (four backends + four same-named casts; a missing config file reproduces the old
behavior), `[profiles]` / `KAIBO_PROVIDER` / `--provider` are loud tombstones.

Review-pass fixes landed: per-call overrides take a verbatim model id plus an explicit
backend arg (no `"backend/id"` call-arg parsing — a bare HF org prefix must never
silently retarget through a backend alias), tool inputs are `deny_unknown_fields`,
`kaibo://config` renders the alias registries, and a new openai-kind backend must declare
`base_url`. The transitional `provider` call-arg alias rode one cycle and has since been
removed — a stale `provider` is now a loud unknown-field error.

## 2026-06-11 — media-spine foundations

The role table (`[profiles.<name>.models]`, `ModelRole` / `ModelSlot` in `config.rs`;
flat `explorer_model` / `synth_model` keys stay valid, both-spellings is a loud error)
and capabilities-as-data (`ModelCaps` + vision classifier in `consult.rs`, per-slot
`vision` pin, resolved caps rendered at `kaibo://config`). The groundwork vision-in and
image-out built on; nothing in it waited on kaish.

## 2026-06-10 — path containment + config discovery

Always-on path containment with launch-cwd default, a `kaibo://config` resource, and a
scope section in the server instructions. Every call's path must canonicalize (symlinks,
`..` resolved) into the allowed set, enforced in `server.rs::resolve_root` with tests in
`tests/containment.rs`.

## 2026-06-08 — host-env hermeticity retired (now wholly structural)

The kaish-side fixes this entry tracked all landed: tilde `~` / `~/path` and bare `cd`
now consult the kernel scope `HOME` (kaibo seeds none, so they stay literal — no
host-path disclosure), `~user` / `/proc` are `host`-gated (off here), and a new
structural guard makes any `with_backend` kernel refuse host side channels — output spill
is forced in-memory and background-job output files are suppressed, so neither bypasses
the read-only mount onto the real filesystem. The read-only invariant is now wholly
structural.

## 2026-06-08 — offline mock harness

A scripted `CompletionClient` (`src/test_support.rs`, content-driven so it's robust to
rig's `buffer_unordered` tool execution and the finalize replay) now drives the *real*
consult loop with no network. Closed two test-gap entries: an e2e proving a `consult`
`explore` tool call genuinely drives the nested `explore′` agent and aggregates into
`ConsultOutput.report`, and the session record/replay glue.

That glue moved out of `server.rs` down past the provider macro into a generic
`consult::consult_session_turn` — `consult()` now owns sessions (`Option<Session>` =
stateless one-shot when `None`), so the history→consult→record dance is offline-tested,
including the "a failed turn records nothing" invariant. Bonus: turn-cap
`finalize_after_max_turns` recovery is now offline-tested too.

## 2026-06-08 — read-only denylist retired (now wholly structural)

kaish landed the upstream fix: `touch` routes its mtime bump through a `set_mtime`
backend op the read-only mount rejects; `mktemp` resolves its parent through the VFS, so
it lands in ephemeral `MemoryFs`, never real `/tmp`. kaibo dropped the hardcoded
`DENYLIST` entirely; the read-only invariant is now wholly structural (mount + MemoryFs +
compiled-out axes). `Blocked` survives only as the engine for config-driven
`[sandbox].disable_builtins`. Tests reworked to assert structural teeth and proved by
mounting the project writable.

## 2026-06-08 — explorer report surfacing

`consult`'s `ConsultOutput.report` now rides as `structured_content` when a call sets
`include_report`, off by default so a normal consult stays lean; `server.rs`
`consult_result` is the pure, offline-tested seam.

## 2026-06-07 — turn-cap graceful degradation

`consult.rs`: `MaxTurnsError` is no longer fatal, since rig 0.34 hands back the full
transcript; `run_phase` now forces one final `ToolChoice::None` answer-now turn from the
partial work, and caps were raised to explorer 100 / synth 200 now that hitting them is
no longer fatal. The prior entry's premise — that rig discards the transcript — was
stale.
