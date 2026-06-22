# kaibo — Known Issues & Open Work

解剖（かいぼう）'s punch list. kaibo is an assistant agent for other agents — a team
of models offering *consultation* (read-only, cited codebase answers) and
*capabilities* (image generation today, more later). This file is where we record
what's missing, what's fragile, and what we'd improve. Evidence-first — name the
file, the line, the *why*, and how it surfaced.

Conventions:

- **Delete entries when they ship.** Don't mark them done — remove them. The
  *reasoning* behind a ship lands in `docs/devlog.md` (dated, why-not-what); this
  list stays open-work-only so it's cheap to skim before proposing new work.
- Narrative/architecture context lives in code doc-comments and project memory
  (`kaibo-architecture`, `kaish-readonly-bypass`, `provider-model-ids`).
- Priorities: **P1** high-leverage features & robustness · **P2** focused
  fixes & hardening · **P3** infra, perf, polish · **P4** eventually.

History of shipped work moved to `docs/devlog.md` (2026-06-18). Newest entry there:
kaish-kernel 0.9.0.

---

## P1 — High-leverage features & robustness

### Media spine + pal merge: vision-in first, production tools as kaish builtins
Direction settled 2026-06-10 (conversation w/ Amy): kaibo absorbs the pals' model
tools over time — image gen/image2image, tts, eventually more — specialized via
config the way models already are. The user asks for it all in one place; the shell
is the workflow layer. Rationale recorded here so it survives the conversation.

**Architecture rules (the cheap decisions made now):**

- **Perception vs production.** A tool whose output the model must *see*
  (`view_image`) is a **rig tool** — the only channel that carries image parts into
  model context is the rig tool-result envelope (`ToolResultContent::from_tool_output`
  parses `{"response":…, "parts":[{"type":"image","data":…,"mimeType":…}]}` — the
  part key is camelCase **`mimeType`**, confirmed against rig-core 0.34
  `completion/message.rs:888` (the `mime_type` spelling in an earlier draft of this
  note was wrong); the Anthropic and Gemini arms map image parts natively both
  directions, `providers/gemini/completion.rs:455,1135`). A tool whose output is an
  *artifact* is a **capability** — its own MCP tool, not a `run_phase` loop.
  **Refined 2026-06-13 (image-out shipped):** the basic "agent asks kaibo to *make*
  something" path is a direct capability tool (`generate_image`: resolve slot →
  provider call → `Content::image`), simpler and the natural agent-facing surface.
  The earlier plan to make these **kaish builtins** (async `Tool::execute` +
  `register_arc`, like `Blocked` in `sandbox.rs`) was premised on *shell composition*
  — `image2image <in> <out>` piped to another tool, artifacts on the scratch VFS. That
  rationale is real but it's a *later* concern (media pipelines), re-homed to the
  deferred follow-on below; don't reach for a builtin until composition is the point.
- **Media moves by VFS path; base64 only at the edges** (provider wire, MCP
  content). Scratch `MemoryFs` is the working bus — bounded now that kaish landed
  `ByteBudget` (kaish-vfs, 2026-06-10; kaibo pickup tracked in the P2 entry below).
  A future `--out-dir` adds a third mount: project **ro** / scratch **rw-bounded** /
  `/out` **rw** mapped to a user-specified real directory, off by default.
  Read-only-is-the-product survives precisely: the project mount stays ro; bytes
  land on the real filesystem only where the user explicitly pointed. Failing-first
  tests when it lands: writes outside `/out` refused; project ro even with out-dir set.
- **Delivery over MCP:** small images inline as `Content::image` (rmcp 0.16,
  `model/content.rs:165`); large objects flush to `/out` and return as
  `RawContent::ResourceLink` (`model/content.rs:140`) instead of base64 blobs.
- **Capabilities are data on the `ModelShape` seam** — SHIPPED
  2026-06-11: `ModelCaps` + the vision classifier (`consult.rs`), per-slot
  `vision` pin in the role table, resolved caps at `kaibo://config`. The vision
  half of the *consumption* shipped too (vision-in, see the Last pass): a vision
  arm's toolset gains `view_image`. The `image` production role is now consumed too
  (image-out, 2026-06-13): `generate_image` reads `slot(ModelRole::Image)` and is
  refused honestly off an unconfigured cast — no kernel-conditional builtin needed,
  since it's a direct tool, not a builtin.
- **Roles outgrow explorer/synth** — SHIPPED 2026-06-11: the role table
  (explorer, synth, image, tts; `ModelRole`/`ModelSlot` in `config.rs`),
  spelled `[casts.<name>]` since the backends/casts split shipped. The `image` slot
  is now consumed by `generate_image`; `tts` stays a reserved seam pending rig
  coverage. `consult`/`oneshot` are the agent costumes over `run_phase` (the old
  `explore`/`synthesize` tools folded into them — see the surface-collapse below);
  capabilities are their own tool shapes, never new loops.

**Sequencing:** (0) the backends/casts split — SHIPPED 2026-06-11. (1) vision-in —
SHIPPED 2026-06-11, path-only. (2) **image-out — SHIPPED 2026-06-13** as the
`generate_image` capability tool (live-verified; see the top Last pass), inline
`Content::image` only. **Next: (3)** the deferred large-artifact + composition work —
`--out-dir` (project **ro** / scratch **rw-bounded** / `/out` **rw**, off by default;
failing-first tests: writes outside `/out` refused, project ro even with out-dir set)
+ `ResourceLink` delivery for objects over the inline cap, and the kaish-builtin/VFS
surface for *composition* (image2image, feeding a generated artifact to another tool in
a `run_kaish` script). Those builtins need the per-builtin timeout (its own P1 entry)
before a minutes-long model call. Additive after that.

**Open design points (for the production builtins):** session history records
`[image: path, mime]` markers, not blobs; the input size cap is a `view_image` const
today (`DEFAULT_MAX_IMAGE_BYTES`, 5 MiB) — promote to a `[sandbox]`/`[defaults]` knob
if it bites. **Explicitly deferred:** inline/attach (non-file) image *input* (YAGNI —
add when a genuinely-never-a-file pasted image comes up), search/code-exec tools,
file-store/context-cache plays, batch synth (its P3 entry stands), any
image-processing crate.

**TTS/STT — PARKED pending rig provider coverage (decided 2026-06-13).** No sound
devices in scope: file-in/file-out only (TTS writes an audio file, STT reads one and
returns text — `stt` is the natural fit for a kaish builtin emitting text, no new
delivery channel; TTS is the artifact path needing out-dir + per-builtin timeout).
The blocker is rig, not kaibo's design. rig 0.38 *has* the traits
(`AudioGenerationModel` = TTS, `TranscriptionModel` = STT) but coverage is uneven:
- **TTS** — openai-kind only (also xai/azure/openrouter); **no Gemini, no Anthropic,
  no DeepSeek**. So the obvious chimera "voice on Gemini" can't be driven through rig.
- **STT** — openai-kind **and Gemini** (also hf/mistral/groq/azure); no Anthropic/DeepSeek.
- Feasibility note for the adopter: rig's openai audio/transcription methods hang off
  `openai::Client`, but kaibo builds `openai::CompletionsClient` (`consult.rs:637`) —
  a second small client on the same base_url/key, not a blocker.

Decision: **wait for rig to broaden coverage and adopt its traits wholesale**, rather
than hand-roll Gemini's AUDIO-modality `generateContent` over raw HTTP now (a second
non-rig wire path to maintain, against the one-primitive grain). Kept as ready seams:
`ModelRole::Tts` still parses/resolves into a cast slot (annotated reserved in
`config.rs`); `stt` isn't a role yet (add with the consumer). The shipped
`config.example.toml` was scrubbed of the `tts` slot — the embedded template must not
advertise a capability kaibo lacks; `docs/config.md`/`docs/casts.md` document the
reserved roles honestly. **When this un-parks** (rig adds Gemini/Anthropic TTS, or
openai-only TTS is judged enough): wire the `tts` builtin (needs the per-builtin
timeout + out-dir below), add the `stt` role + builtin, restore the example slots.

### Per-builtin timeouts: the 30s script timeout cannot serve model-backed builtins
The kernel exec timeout is one global knob: `KAISH_EXEC_TIMEOUT` (30s,
`sandbox.rs:103`) threaded via `with_request_timeout` (`sandbox.rs:184`),
overridable only wholesale (`[sandbox].exec_timeout_secs` /
`KAIBO_EXEC_TIMEOUT_SECS`, `config.rs`). 30s is *way* too small for complex
pro-model calls — image gen and pro-tier completions routinely run minutes — so
every production builtin (image2image, tts; P1 media-spine entry above) would be
killed mid-flight with exit 124. But the fix is not raising the global: that one
knob is doing two jobs — killing runaway scripts (30s is right) and bounding
provider patience (30s is wrong) — and stretching it to minutes hands a
`while true` loop the same minutes.

Fix shape: split the jobs. Model-backed builtins get their own timeout budget
(per-builtin or per-tool-class, config-overridable, generous default — minutes,
not seconds), aligned with the per-backend `request_timeout` already governing
rig's HTTP calls so the kernel never undercuts a legitimate in-flight provider
call; plain script execution keeps the tight 30s. Mechanism question answered
2026-06-11: enforcement is a kernel-side watchdog, strictly per-execute — a
timer task sleeps the whole duration and fires the cancel token (kaish
`kernel.rs:1511,1618-1625`); `ExecuteOptions.timeout` resizes per-script but
nothing can suspend it mid-script, so this *does* need a kaish-kernel seam.
The upstream seam **shipped** (kaish 0.8.2/0.8.3): `ctx.patient(budget) ->
PatientGuard` on `ToolCtx` (kaish `watchdog.rs`, a `timeout` builtin), a movable
deadline whose cancel surface stays live while suspended. So the blocker is
cleared — but kaibo has no in-kernel model-backed builtin to wire it onto yet
(production capabilities ship as MCP tools, not kaish builtins). kaibo's half
lands *before or with* the first production builtin;
failing-first test: a builtin that sleeps past 30s but under its own budget
completes, while a pure-script spin still dies at 30s.

---

## P2 — Focused fixes & hardening

### Review the provider retry + failure policy (and document it)
We don't have a stated, audited answer for what a `consult`/`oneshot`
call does when a provider misbehaves mid-flight: a 429/529 overload, a connection
reset, a partial stream, or a wedged-but-connected backend. Today the only explicit
guard is the per-backend `request_timeout_secs` (default 900, `config.md`) that bounds
a *single* completion — but whether rig retries with backoff, how many times, and
whether a terminal provider error surfaces to the caller as a clean tool error (so the
host agent can proceed *without* the consultation) versus failing the whole call is
unverified. Surfaced in README review (an SRE reviewer asked "what happens on a
DeepSeek 529?" and we couldn't answer from the docs).

Do: trace the actual path through `consult.rs` → the rig client (`tls.rs`/client
construction) → reqwest; confirm or add retry/backoff with a cap; make a terminal
failure a clean, named tool error rather than an opaque internal_error; then document
the policy in the README FAQ (it currently only promises "a down provider surfaces a
clean error" loosely) and `docs/config.md`. A failing-first test that injects a 529 via
the scripted `CompletionClient` (`test_support.rs`) is the natural guard.

## P3 — Infra, perf, polish

### Expand the `kaibo://config` `[runtime]` section beyond followed worktrees
The config resource grew a `[runtime]` table for state that's *computed at read
time* rather than configured — currently `follow_worktrees` (the knob) and
`followed_worktrees` (the live extra set the worktree-follow grants beyond
`allowed_paths`, recomputed each read so a mid-session worktree shows up). The slot
is the right home for other runtime-derived facts a caller/operator would want to
see at a glance — candidates: which casts actually resolved a key right now (vs.
merely configured), the resolved default root's *source* (explicit vs. inferred
cwd) surfaced inside `[runtime]` instead of as a sibling flag, live session-store
occupancy, or a "git repo detected at root" hint. Add these as the need shows up;
keep the rule that `[runtime]` is *observed*, not *set* (the static knobs stay
above it), so a reader can always tell "what kaibo discovered" from "what the
operator chose". `RuntimeDoc` in `server.rs::render_config_resource` is the seam;
the value is threaded in from the handler (it needs `allowed_set` / live state),
so new fields follow that same path.

### `[context]` house rules have no size cap (and ride every turn, every phase)
The `[context]` files are spliced into the preamble whole (`context.rs::assemble`
→ `consult.rs::with_house_rules`), the preamble is re-sent on *every* model turn,
and the block rides every codebase-reading phase — the `consult` driver and each
nested `explore′` sweep (the toolless `oneshot` reads no project, so it gets none).
A large `AGENTS.md` +
`~/.claude/CLAUDE.md` is real token cost multiplied across turns *and* sweeps. No
truncation by design — silent truncation of operator guidance is the wrong failure
— but a generous cap with a *loud* error (or a startup warning naming the byte
count) would catch a runaway file before it quietly bloats every call. Measure a
real config before adding the knob. Project-local `.kaibo.toml` layering (already
noted under "File location") would let a repo ship its own `[context]` without a
global edit.

### Multi-turn session history is unbounded per session
`SessionStore` (`session.rs`) caps the number of *sessions* (LRU, `session_capacity`)
but keeps every `(question, answer)` pair in a session forever — matching dpal, which
also doesn't cap per-thread depth. The pairs are lean, but a very long-running thread
grows `consult_user_prompt`'s context monotonically; a local model's small window
(see the context-window entry below) would feel it first. If it bites, keep the last
N turns (or summarize older ones) rather than all — but measure a real long session
before adding the knob.

### Two kernel builds + two threads per consult
Each phase spawns a fresh `KaishWorker` (explorer + synth = two OS threads, two
read-only kernel builds) so the synth starts clean at the root (`consult.rs`). Fine
for now, but a busy server rebuilds kernels constantly. Consider a small worker
pool, or resetting one kernel's cwd between phases instead of rebuilding. Measure
before optimizing.

### Flaky tracing-capture tests in `tool_span.rs`
`emits_an_error_outcome_when_the_tool_fails` (and its `ok` sibling) fail ~25% of
the time in the *parallel* full `cargo test` run, never in isolation. Both register
a `tracing_subscriber::registry().with(cap)` via `set_default` and assert on captured
spans; run concurrently on cargo's test threads they race on tracing's process-global
span store, so one test's `tool` span can go missing from the other's capture. A test
that fails when we *didn't* make a mistake is the opposite of the teeth we want. Fix:
serialize the two (a shared `Mutex`/`serial_test`), or capture per-span without leaning
on global dispatch. Out of scope when found (the read-idioms/output-cap PR).

### Batch — remaining providers and the many-casts fork
The batch tool class shipped Anthropic- and Gemini-first (`src/batch.rs`,
`batch_submit`/`batch_get`/`batch_cancel`/`batch_list`, one `--no-batch` gate): offline,
toolless, max-effort fan-out behind the `BatchProvider` seam, with the design rationale
in the module doc and `docs/devlog.md`. What's left:

- **OpenAI batch** — file-based (upload JSONL, reference a file id, poll, download an
  output file), unlike Anthropic's inline POST. The output file is left in place by
  default; add a `config.toml` flag to opt into cleanup for callers who'd rather not
  accrete files. The generic local `openai` kind stays ✗ (no batch endpoint).
- **The many-casts fork.** `batch_submit` today is many-prompts/**one**-cast (one
  provider batch). The diverse-opinion panel — one question across **many** casts — is N
  provider batches under a composite handle; deferred. The provenance footer already
  makes each result self-labelling, so the rendering is mostly there.
- **Effort tier.** Batch forces `BATCH_EFFORT = "high"` (== `DEFAULT_EFFORT`, the
  proven-accepted top for the Anthropic adaptive tier). If a higher tier (`xhigh`/`max`)
  is ever confirmed by probe for a batch backend, lift it there — the constant is the
  one knob to change.

Per-provider capability, `None` where unsupported: Anthropic ✓ (shipped), Gemini ✓
(shipped — inline batch, `gemini-batch` cast synths Pro), OpenAI ✓ file-based (next),
DeepSeek ?, local `openai` ✗.

### Batch design hardening (cross-model Opus review, 2026-06-22)
A cross-family review of the batch slice (Opus 4.8, run *through* `batch_submit` itself
dogfooding the tool) confirmed the bones — the verb surface, stateless
`backend/provider-id` handle, per-item failure surfacing — and flagged four follow-ons.
Shipped in the same pass: the batch preamble (no longer a verbatim `oneshot` reuse —
`consult.rs::batch_preamble`, overridable via `[prompts].batch`), the `/`-in-backend-name
guard the handle split relies on (`config.rs`), the don't-busy-wait note on the tool
descriptions, and **`batch_list`** (`server.rs`/`batch.rs::list`) — the way back to a
batch whose handle was lost, closing the orphaned/billing-batch footgun (per-backend
failures and a truncated page are surfaced, not hidden). Still open:

- **Index-as-`custom_id` + statelessness = context-loss risk.** Result labels are bare
  `0..N`, meaningful only to a caller still holding the ordered prompt list. Echo the
  prompt (or a digest) back beside each answer in `batch_get` so a result is
  self-describing — the natural complement to holding no state. (`batch.rs::render_poll`
  / `BatchAnswer` is the seam; would need the submitted prompts carried or re-fetched.)
- **`batch_get` returns prose for both pending and done.** An agent must parse the
  progress string to know whether it's looking at a status or its answers. A structured
  status token (or distinct content shape) would let the caller branch without reading
  prose.
- **Forced effort vs. a floor.** `max_tokens` is already a floor (never undercuts a
  richer slot), but effort is *force-clobbered* to `BATCH_EFFORT` — lossy for legitimate
  bulk-classification / short-extraction batches where high effort is wasted spend.
  Consider making effort a default-on floor the cast/caller can lower, the way
  `max_tokens` already is. (Distinct from the "lift the tier" bullet above — that's the
  ceiling, this is the override.)

### Provider model ids drift and live in code
`consult.rs::default_models` hardcodes the explorer/synth ids per provider; they
rot (rig 0.34's bundled `CLAUDE_*` / gemini consts are already retired — see the
`claude-3-5-haiku-latest` 404 on 2026-06-03). Keep them in sync with the
source-of-truth pal configs (`provider-model-ids` memory). → Model ids now live
on cast slots and are overridable per cast in `config.toml` (shipped;
`docs/config.md`). The in-sync-with-pals discipline for the built-in defaults
(`config.rs::default_models`) stays regardless.

### rig provider gaps we route around (tracked upstream limitations)
`rig-core` is the wire layer for every provider; where it's thin, kaibo inherits the
gap. We adapt rather than fork, but the cost is real — record it so we don't keep
rediscovering it. Two live ones beyond the TTS coverage matrix (see the media-spine
P1 entry):

- **openai image gen drops `additional_params` — no SD knobs reachable.** rig 0.38's
  `providers/openai/image_generation.rs::image_generation` hardcodes the request body
  to `model`/`prompt`/`size` (+`response_format` for non-gpt-image) and **never
  serializes the request's `additional_params`** — the field exists on
  `ImageGenerationRequest` and the builder sets it, but this impl ignores it (the
  completion path *does* merge it; the image path doesn't). So every Stable-Diffusion
  knob — steps, cfg_scale, sampler/scheduler, seed, negative_prompt, clip-skip,
  **LoRA weights** — is dropped before it leaves the process. Confirmed not a server
  limit: a direct POST to lemonade's `/api/v1/images/generations` with
  `steps`/`cfg_scale`/`seed`/`negative_prompt` returned an image (2026-06-13 probe) —
  the wire honors extras, rig won't send them. *Exception:* sd-cpp-style LoRA via
  `<lora:name:weight>` rides in the `prompt` string, which rig **does** send, so that
  subset may already work (unconfirmed — needs a differential probe). **Landmine:**
  don't add a `params`/`negative_prompt` arg to `generate_image` until the wire
  carries it — accepting a knob rig silently drops is exactly the silent fallback we
  refuse. Fix path: upstream a one-spot merge of `additional_params` into the images
  body (small, obviously-correct), then expose params as a per-call arg + per-slot
  defaults (the `ModelShape`/tunables shape); **not** a hand-rolled images POST (a
  second non-rig wire path, the TTS lesson). Until then, `generate_image` is
  prompt-only by design, not omission.
- **Gemini support is thin — text in, little else.** rig 0.38's gemini provider is
  `Completion` + `Embeddings` + `Transcription` (STT) + `ModelListing` only:
  `client.rs` declares `type ImageGeneration = Nothing` and `type AudioGeneration =
  Nothing` for *both* the standard and interactions-API clients. So Gemini — a richly
  multimodal family — reaches kaibo as essentially a text/vision-in completion
  backend; rig exposes none of its image gen (Imagen/"nano-banana"), TTS, context
  caching, file stores, or search/code-exec grounding (all of which the `gpal` MCP
  sibling drives directly). This is why the example casts carry commented-out
  `gemini/...image` ids as TODOs that can't yet land, and why "voice on Gemini" parked
  with TTS. Track rig's gemini coverage; adopt its traits when they broaden rather
  than hand-rolling Gemini's `generateContent` media modalities over raw HTTP.

### Per-model request shaping (`ModelShape`): remaining knobs
`ModelShape` (`consult.rs`) resolves request params per (kind, model), fit per
*arm* with the slot's tunables via `Arm::from_slot` (each falling back to the
per-role `[defaults]`). Thinking is model-aware across all providers (Anthropic
adaptive vs enabled-budget, Gemini 3-line `thinkingLevel` vs 2.5/3.5
`thinkingBudget`), reasoning depth is per-role effort (with `thinkingLevel` as
the 3-line's effort sink), and `thinking_style` (per slot or `[defaults]`)
overrides the Anthropic classifier. `max_tokens`/`thinking_budget` are per-slot
tunables — if a provider caps output low, cap that slot, not the global, per the
`large-token-headroom` memory. Remaining knobs on the same seam:
- **Gemini 3.5 boundary is empirical.** The classifier (`is_gemini3_level`) flips
  only the pure `gemini-3-*` line to `thinkingLevel`; `gemini-3.5-flash` stays on
  budget because the 2026-06-06 live test confirmed budget works there. If a future
  3.5 build *rejects* budget, widen the classifier — but confirm with a live probe,
  don't guess.
- **Anthropic adaptive boundary is empirical.** `is_anthropic_adaptive` flips Opus
  4.6+/Sonnet 4.6/Fable 5 to adaptive (the rest stay enabled-budget). Add ids as models
  ship, or set `thinking_style` on the slot to override; confirm a new id with the
  `#[ignore]`d Opus-4.8 probe rather than guessing.

All four provider paths have opt-in live tests (`tests/consult.rs`, `#[ignore]`d,
gated on a key/endpoint) and passed with thinking on — the probes above extend these.

### Tunables with no sink are accepted (and rendered) silently
A slot whose resolved `ModelShape` has no sink for a knob still accepts it,
load-validates it, and renders it at `kaibo://config` as if effective:
`thinking_budget` on a Gemini 3-line slot (the level line never sends a budget,
yet the `< max_tokens` inversion check still applies to it), and
`effort`/`thinking_budget` on an openai-kind slot (`ThinkingStyle::None` sends
neither). The effort half of this was fixed for the 3-line (it now maps onto
`thinkingLevel`); the rest is invisible-no-op residue. Fix shape: at load (or in
the render), flag per-slot tunables the slot's resolved shape will never send —
a note in the render is enough to make the no-op visible to the operator.

### Explorer prose — residual probes (the report shape + reading strategy shipped)
The structured report sections (`SummaryOfFindings`/`RelevantLocations`/
`ExplorationTrace`), the curiosity + completeness behaviors, and the assertive
whole-file / `grep -B/-A` reading strategy now live in `report_preamble` (and the
`grep`/`wc -l` idioms in the shared cheatsheet). Measured against a real review task,
a lite Gemini explorer dropped from 48 turns to ~21 with *better* citations — the
built-in reproduces it with no per-cast config. Still open, lower value:
- **A worked, filled-in example in the prompt.** We ship the section *template*, not a
  filled `RelevantLocations` example. A *shown* example may lift the weakest local
  models further — probe if a Gemma explorer underperforms a Gemini one on the shape.
- **`<scratchpad>` scaffold** — deliberately *not* adopted (it pulls toward long chats,
  against the turn cap). Reconsider only for a notably less self-directed local model.
- **Debug affordance:** dump the *assembled* preamble (built-in/override + house rules)
  to a file, à la gemini-cli's `GEMINI_WRITE_SYSTEM_MD` — useful now that prompts
  compose per model from `[prompts]`, slot `preamble`, and `[context]`.

### `generate_image` doesn't advertise its cast `enum`
The consultation tools stamp the usable-cast roster onto their `cast` param as a
JSON-Schema enum (`inject_cast_enum`, `server.rs`); `generate_image` doesn't. Its
`cast` selects the **`image`** slot (openai-kind only), so the right menu is a
different filter — "casts with a usable image slot", not the explorer/synth
`usable_casts` — i.e. a `Config::image_capable_casts` to write. Add it so image gen
is as discoverable as consultation. Advisory like the others (serde ignores the enum
in `call_tool`), so it never rejects a config-only cast.

### Server doesn't validate backend health at startup
Usable *casts* are now advertised — the handshake `## Casts` list and the `cast`
schema enum (`inject_cast_enum`, `server.rs`) tell a client what teams it can pass.
The remaining gap is backend *health*: keys resolve lazily at call time and an
`openai`-kind `base_url` is never pinged, so a present-but-wrong key or a down local
endpoint still surfaces as a mid-call error. A startup check (key presence + an
`openai` `base_url/models` ping) would fail faster and, under casts, report degraded
teams up front — "cast `chimera` is degraded: backend `sd` unreachable" beats a
mid-consult error. An MCP resource enumerating models could ride along.

---

## P4 — Eventually

### Config-overrideable system prompts — residual (the phase-preamble override shipped)
The phase **preambles** are now config-overridable: `[prompts].<phase>` (`explorer` /
`consult` / `oneshot`) and the per-slot `preamble` full-replace the built-in, resolved
through `ConsultConfig.prompts` and threaded into every phase fn (`config.rs`,
`docs/config.md`). Granularity (server-wide *and* per-slot), replace scope (full
replace; the kaish contract still rides the `run_kaish` tool description), and source
(inline `"""…"""`) all landed. What's *not* reachable, by design: the per-call framers
(`consult_user_prompt`), `FINALIZE_NOTE`, and the shared cheatsheet in `kaish_syntax.rs`
(`KAISH_SANDBOX_ADDENDUM` + the `kaish-help`-sourced contract) — the cheatsheet is the
grounding/exit-code contract and must not be silently droppable. Only build a config
path to the framers if a real user wants it; the remaining fork is whether a `*_file`
source (prompts as real files) is worth it alongside the inline form.

### A secrets-manager key source (deferred)
Custom credential paths shipped — a backend's `api_key_env` / `api_key_file`
override the built-in `~/.anthropic-key.txt` / `~/.deepseek-key` / `~/.gemini-api-key`
defaults (`credentials.rs`, `docs/config.md`). A secrets *manager* is still out of
scope: by design the TOML references keys, never inlines them, so "point at
`$SECRET_TOOL` output" would be a future key-source variant alongside env/file.

### OTLP logs + metrics signals (deferred — traces shipped)
The **traces** signal is in: `[telemetry]` (off by default) stands up an OTLP/HTTP
exporter and a `tracing-opentelemetry` layer in `main.rs`, exporting the GenAI span
tree rig already emits (`src/telemetry.rs`, `docs/config.md`). Two signals remain:
- **Logs** — kaibo's `tracing` events (the `kaibo`-target log lines) as an OTLP
  *logs* signal via `opentelemetry-appender-tracing`, a third layer in the same
  registry stack. Today they still ride stderr + the MCP `notifications/message`
  bridge only; nothing exports them.
- **Metrics** — rig records token usage as span *fields*, not as metric
  instruments. Real counters/histograms (tokens, per-phase latency, sweep fan-out)
  are hand-rolled, or derived from spans at the collector. Decide which before
  adding an `opentelemetry` metrics provider.
Both reuse the same off-by-default `[telemetry]` gate and endpoint; the open
question is whether the content/cost of a logs signal is worth it given traces
already carry the prompts/completions. The session's `otlp-mcp` collector is the
sink for a probe.

