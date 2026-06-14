# kaibo — Known Issues & Open Work

解剖（かいぼう）'s punch list. kaibo is an assistant agent for other agents — a team
of models offering *consultation* (read-only, cited codebase answers) and
*capabilities* (image generation today, more later). This file is where we record
what's missing, what's fragile, and what we'd improve. Evidence-first — name the
file, the line, the *why*, and how it surfaced.

Conventions:

- **Delete entries when they ship.** Don't mark them done — remove them. Git
  history is the record. Skim this list before proposing new work.
- Narrative/architecture context lives in code doc-comments and project memory
  (`kaibo-architecture`, `kaish-readonly-bypass`, `provider-model-ids`).
- Priorities: **P1** high-leverage features & robustness · **P2** focused
  fixes & hardening · **P3** infra, perf, polish · **P4** eventually.

Last pass: 2026-06-13 (image-out SHIPPED, **live-verified** — kaibo's first
*capability* (vs. consultation). `generate_image` (`generate_image.rs` + a `server.rs`
handler) is a dedicated MCP tool, **not** a `run_phase` costume and **not** a kaish
builtin: it resolves the cast's `image` slot into an `ImageGen` (`image_gen.rs`),
calls rig's openai `ImageGenerationModel`, sniffs the MIME (shared `view_image::sniff_mime`),
and returns the bytes inline as `Content::image` + a caption. Openai-kind only — rig
0.38 has no image path for the keyed Anthropic/Gemini/DeepSeek protocols, so a
non-openai `image` slot is refused loudly (the same honesty as parked TTS); enabled by
rig-core's `image` feature (zero extra deps). Gated `--no-generate-image` /
`KAIBO_NO_GENERATE_IMAGE`, all-off still refused at startup. Inline-only with a size
cap (`DEFAULT_MAX_IMAGE_BYTES`); over-cap is a loud error, never a silent drop. Offline
tests cover parse/sniff/cap/content + the openai-only resolver gate + tool gating; the
**live probe** (`tests/image_gen_live.rs`, `#[ignore]`) generated a real 569 KB PNG via
local lemonade `SD-Turbo-GGUF` over `/v1/images/generations` in ~9s — so this is
live-works, not just offline-green. **Surface change from the plan below:** image gen
was scoped as a *kaish builtin* (for shell composition); we shipped it as a capability
tool instead — the basic "agent asks for an image" path wants a direct call, and the
builtin/VFS-composition surface is re-homed under image2image/media pipelines, deferred.
Deferred follow-ons: `--out-dir` + `ResourceLink` for large artifacts; per-builtin
timeout (moot for a direct tool — the per-backend `request_timeout` governs); the
builtin/VFS composition surface; non-openai image kinds pending rig coverage.)

Last pass: 2026-06-12 (view_image on OpenAI-compatible VLMs SHIPPED, offline-green —
the channel fix from `docs/oai-images.md`. An `openai` vision slot now genuinely *sees*:
`view_image` still produces the tool-result image envelope, but on a transport that
can't carry it (the OpenAI wire forbids an image in a `role:tool` message; rig 400s
first), `run_phase` (`consult.rs`) installs a `ViewImageBreakHook` that flags on
`on_tool_result` and terminates on the **next** `on_completion_call` — the turn
boundary where rig's transcript already holds every tool result of the triggering
turn, so co-tool-call orphaning is structurally impossible (verified against rig 0.38.2
`prompt_request/mod.rs:665-672,1081`). The `PromptCancelled` transcript is rewritten
(`rewrite_view_image_history`): each `view_image` result becomes a text ack and a
*separate*, tool-result-free `Message::User { [Image] }` lands after it (mixed in one
turn, rig's openai converter silently drops the image — the load-bearing S2 result),
then the loop resumes via the `finalize_prompt`-style split with a transcript-derived
outer turn budget so a looping `view_image` can't refresh `max_turns`. Gated on a new
see-∧-transport predicate: `ModelCaps.tool_result_images` (= `transport_supports_tool_
result_images(kind)`); anthropic/gemini keep the tool-result channel untouched. Offline
tests: pure rewrite (separate-message, idempotency, co-tool-call selectivity) + two
driven loop tests (single break→resume asserts a user image and *no* tool-result image
copy; a co-tool-call view_image+run_kaish turn resumes cleanly). **OPEN — the live
probe is load-bearing, not optional:** the scripted mock returns its answer regardless
of wire validity, so a rewrite that left an orphaned `tool_use` passes offline; only a
real openai-compatible VLM (local Qwen-VL via llama.cpp/vLLM) reporting a detail it
could only see confirms it. Run before calling this done — see `docs/oai-images.md`
"Tests/Live probe".) 2026-06-11 (vision-in SHIPPED — `view_image(path)` (`src/view_image.rs`):
a vision-capable phase reads an image *file* from the workspace and the bytes reach
model context as a rig image part. Path-only by decision (debug screenshots/assets/
docs are files already in the tree); no MCP-native/inline input. Bytes are read
through the project VFS via a new `KaishWorker::read_file` (a `Job::Read` on the
worker thread → the *project* `VfsRouter`, retained from `build_readonly_kernel_and_vfs`
because under `with_backend` the kernel's own `vfs()` carries only `/v/*` scratch),
so containment + read-only stay structural and the 8 KB script-output cap is bypassed
for the deliberate read. Toolset assembly gates `view_image` on `arm.caps.vision` in
all three phases (`phase_tools` for explore/synthesize, `consult_tools` for the synth
driver; the explore′ sweep inherits its own gate); a blind model never sees the tool,
so there's no fail-loud attach path. Two correctness landmines caught by reading
ground truth, not guessing: rig's part key is camelCase **`mimeType`** (not the
`mime_type` an earlier note claimed), and `Tool::Output` must be a `serde_json::Value`
object — rig `serde_json::to_string`s the output, so a `String` arrives double-encoded
and `from_tool_output` treats it as text, never an image (the offline round-trip test
proves the whole chain). Out-of-workspace paths get an actionable copy-it-in error
(the caller's agent can act on it); MIME by magic-byte sniff; a loud size cap, no
resize dep. Offline tests: tool unit tests + the full caps→toolset→VFS→envelope→rig
round-trip + a blind-synth negative. Folded out sequencing step (1) below.) 2026-06-11 (kaish 0.8.1 bump + scratch ByteBudget SHIPPED — the dep
moved to published `kaish-kernel = "0.8.1"` (clean, no API breakage), and the
unbounded-`/`-scratch surprise is closed: `[sandbox].scratch_limit_bytes` (env
`KAIBO_SCRATCH_LIMIT_BYTES`, default 64 MB, must be > 0 — no "unbounded" escape)
threads an owned labeled `ByteBudget` onto the scratch `MemoryFs` via
`MemoryFs::with_budget` at `sandbox.rs` construction, so a runaway redirect fails
loudly (`StorageFull`) instead of eating host RAM for the kernel's lifetime.
`ByteBudget` rides `kaish_kernel::vfs` — no direct kaish-vfs dep. Failing-first
test in `tests/sandbox.rs` (proven teeth by swapping in an unbounded mount). Folded
out the P2 entry. Bonus from the bump: kaish fixed "redirects ignore cwd", so a
bare relative `> f` now resolves against cwd — the sandbox tests target absolute
scratch paths accordingly. A DeepSeek review of the backends/casts config also
landed one fix: the table slot form now trims `backend`/`id` like the string-ref
form, so identical intent doesn't hinge on spelling). 2026-06-11 (backends/casts split SHIPPED — `docs/casts.md` stands as
the design record: `Profile` dissolved into `[backends.<name>]` (connections) +
`[casts.<name>]` (role → `"backend/model-id"`, freely cross-backend), calls
select casts via the `cast` param, each slot resolves to its own `Arm` (client +
request shape) behind the decided vtable seam. Built-in equivalence holds (four
backends + four same-named casts; a missing config file reproduces the old
behavior), `[profiles]`/`KAIBO_PROVIDER`/`--provider` are loud tombstones, and
the review-pass fixes landed: per-call overrides take a verbatim model id plus
an explicit backend arg (no `"backend/id"` call-arg parsing — a bare HF org
prefix must never silently retarget through a backend alias), tool inputs are
deny_unknown_fields, `kaibo://config` renders the alias registries, and a new
openai-kind backend must declare base_url. Folded out the P1 entry; the
transitional `provider` call-arg alias rode one cycle and has since been removed
— a stale `provider` is now a loud unknown-field error). 2026-06-11
(media-spine foundations shipped — the role table
(`[profiles.<name>.models]`, `ModelRole`/`ModelSlot` in `config.rs`; flat
`explorer_model`/`synth_model` keys stay valid, both-spellings is a loud error)
and capabilities-as-data (`ModelCaps` + vision classifier in `consult.rs`,
per-slot `vision` pin, resolved caps rendered at `kaibo://config`). Sequencing
step (2) of the P1 media-spine entry folded out; vision-in is next and nothing
in it waits on kaish). 2026-06-10 (path containment + config discovery shipped — always-on
path containment with launch-cwd default, `kaibo://config` resource, and scope
section in server instructions). 2026-06-08 (host-env hermeticity entry retired — the kaish-side fixes it
tracked all landed: tilde `~`/`~/path` and bare `cd` now consult the kernel scope
`HOME` (kaibo seeds none, so they stay literal — no host-path disclosure), `~user`/
`/proc` are `host`-gated (off here), and a new structural guard makes any
`with_backend` kernel refuse host side channels — output spill is forced in-memory and
background-job output files are suppressed, so neither bypasses the read-only mount onto
the real filesystem. The read-only invariant is now wholly structural. Folded out the P3
entry). 2026-06-08 (offline mock harness shipped — a scripted `CompletionClient`
(`src/test_support.rs`, content-driven so it's robust to rig's `buffer_unordered`
tool execution and the finalize replay) now drives the *real* consult loop with no
network. Closed two test-gap entries: an e2e proving a `consult` `explore` tool call
genuinely drives the nested `explore′` agent and aggregates into
`ConsultOutput.report`, and the session record/replay glue. That glue moved out of
`server.rs` down past the provider macro into a generic `consult::consult_session_turn`
— `consult()` now owns sessions (`Option<Session>` = stateless one-shot when `None`),
so the history→consult→record dance is offline-tested, including the "a failed turn
records nothing" invariant. Bonus: turn-cap `finalize_after_max_turns` recovery is now
offline-tested too. Folded out both P3 test-gap entries). 2026-06-08 (read-only
denylist retired — kaish landed the upstream fix
(`touch` routes its mtime bump through a `set_mtime` backend op the read-only mount
rejects; `mktemp` resolves its parent through the VFS, so it lands in ephemeral
`MemoryFs`, never real `/tmp`). kaibo dropped the hardcoded `DENYLIST` entirely; the
read-only invariant is now wholly structural (mount + MemoryFs + compiled-out axes).
`Blocked` survives only as the engine for config-driven `[sandbox].disable_builtins`.
Tests reworked to assert structural teeth and proved by mounting the project writable.
Folded out the P2 entry). 2026-06-08 (explorer report surfacing shipped — `consult`'s
`ConsultOutput.report` now rides as `structured_content` when a call sets
`include_report`, off by default so a normal consult stays lean; `server.rs`
`consult_result` is the pure, offline-tested seam. Folded out its P3 entry).
2026-06-07 (turn-cap graceful degradation shipped — `consult.rs`:
`MaxTurnsError` is no longer fatal, since rig 0.34 hands back the full transcript;
`run_phase` now forces one final `ToolChoice::None` answer-now turn from the partial
work, and caps raised to explorer 100 / synth 200 now that hitting them is no longer
fatal. Folded out the prior P1 entry — its premise that rig discards the transcript
was stale).

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
  coverage. `explore`/`synthesize` remain the agent costumes over `run_phase`;
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
Design recorded in kaish `docs/issues.md` ("Watchdog seam: a per-builtin
'patient' budget", targets 0.8.2): movable deadline + RAII `ctx.patient(budget)`
guard on `ToolCtx`, cancel surface stays live while suspended. kaibo's half
waits on that release. Lands *before or with* the first production builtin;
failing-first test: a builtin that sleeps past 30s but under its own budget
completes, while a pure-script spin still dies at 30s.

---

## P2 — Focused fixes & hardening

## P3 — Infra, perf, polish

### `[context]` house rules have no size cap (and ride every turn, every phase)
The `[context]` files are spliced into the preamble whole (`context.rs::assemble`
→ `consult.rs::with_house_rules`), the preamble is re-sent on *every* model turn,
and the block now rides *every* phase — the driver, standalone `explore`/
`synthesize`, and each nested `explore′` sweep. A large `AGENTS.md` +
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

### `synthesize_batch` — a tool-less, batchable synth variant (deferred from the tool-surface work)
The standalone `synthesize` we shipped is *interactive* (`{run_kaish}`) so it can
self-correct a bad cite — the panel was unanimous a tool-less synth is too weak.
The strictly tool-less, non-interactive variant is still worth having for batch
fan-out: submit → poll → read, modelled on gpal/cpal's job system. Uses the parked
`SYNTH_ONESHOT_PREAMBLE` wording in the (now-deleted) plan. It's a **per-provider
capability** like `thinking_params`: Gemini ✓, Anthropic ✓, DeepSeek?, `openai` ✗ —
`None` where unsupported. Open fork when built: many-questions/one-model vs
one-question/many-models (the diverse-opinion panel made literal).

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
whole-file / `rg -B/-A` reading strategy now live in `report_preamble` (and the
`rg`/`wc -l` idioms in the shared cheatsheet). Measured against a real review task,
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

### `generate_image` doesn't advertise its cast `enum` yet
The consultation tools (`consult`/`explore`/`synthesize`) now stamp the live
usable-cast roster onto their `cast` param as a JSON-Schema `enum` at startup
(`inject_cast_enum`, `server.rs`), so an agent picking a team reads the menu off
the schema instead of the truncatable handshake prose — the fix for the "had to
spelunk `kaibo://config` to find `deepseek`" failure. `generate_image` was left
out on purpose: its `cast` selects the **`image`** slot (openai-kind only), so the
right menu is "casts with a usable image slot", not the explorer/synth `usable_casts`
list — a different filter (`Config::image_capable_casts`, to write). Add it so image
gen is as discoverable as consultation. Low-risk: the enum is advisory (`call_tool`
deserializes via serde, which ignores it), so it never rejects a config-only cast.

### Server doesn't report which backends are usable
Keys are resolved lazily at call time, so a missing key surfaces as a mid-call
error. Validating available backends at startup (and noting them in the server
instructions) would fail faster and tell a client what it can actually use — under
casts this gets more valuable, not less: one dead backend can hole several casts,
and "cast `chimera` is degraded: backend `sd` unreachable" is a better failure
than a mid-consult error. For an `openai`-kind backend this means a startup ping
of its `base_url/models` rather than a key check. This can come along with adding
an MCP resource for listing models.

---

## P4 — Eventually

### Config-overrideable system prompts (deferred until a non-Amy user asks)
Note: the in-tree `ModelShape` seam (P3, "Per-model request shaping") is the *same*
resolution point this would extend — build that first; config override rides it.
Every model-facing string is hardcoded in Rust today: the three phase preambles
(`consult.rs` `report_preamble`/`synthesize_preamble`/`consult_preamble`, each
interpolating `kaish_syntax_core()`), the per-call framers (`synthesize_user_prompt`,
`consult_user_prompt`), `FINALIZE_NOTE`, and the shared cheatsheet in
`kaish_syntax.rs` (`KAISH_SANDBOX_ADDENDUM` + the `kaish-help`-sourced contract). No
config path reaches any of them. The seam is already there: `ConsultConfig`
(`consult.rs`) is threaded into every phase fn, so a resolved `prompts` set could ride
on it and replace the `&consult_preamble()` literals at the `.preamble()` call in
`run_phase`; on the config side a `[prompts]` table would slot in exactly like
`[defaults]`. Deferred because Amy is the only user and edits the source directly —
build it when someone else needs it. Three forks to settle *when* asked, not now:
- **Granularity** — server-wide `[prompts]` vs per-cast/per-slot (prompt framing is
  model-sensitive: Gemma fixates on prohibitions where capable models don't, per the
  positive-framing discipline, so per-slot has a real case) vs both.
- **Replace scope** — override the role text only and keep injecting
  `kaish_syntax_core()` (safe: can't silently drop the grounding/exit-code contract)
  vs full-preamble raw replace vs per-field opt-in.
- **Source** — `*_file` path (keeps `config.toml` readable, prompts as real files)
  vs inline `"""…"""` vs both, mirroring `api_key_env`/`api_key_file`.

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

