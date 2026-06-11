# kaibo — Known Issues & Open Work

解剖（かいぼう）'s punch list. kaibo dissects a codebase read-only and answers
questions; this file is where we record what's missing, what's fragile, and what
we'd improve. Evidence-first — name the file, the line, the *why*, and how it
surfaced.

Conventions:

- **Delete entries when they ship.** Don't mark them done — remove them. Git
  history is the record. Skim this list before proposing new work.
- Narrative/architecture context lives in code doc-comments and project memory
  (`kaibo-architecture`, `kaish-readonly-bypass`, `provider-model-ids`).
- Priorities: **P1** high-leverage features & robustness · **P2** focused
  fixes & hardening · **P3** infra, perf, polish · **P4** eventually.

Last pass: 2026-06-11 (backends/casts split SHIPPED — `docs/casts.md` stands as
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
`provider`-alias removal note moved to its own P2 entry below). 2026-06-11
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
  parses `{"response":…, "parts":[{"type":"image","data":…,"mime_type":…}]}` —
  rig-core 0.34 `completion/message.rs:864`; the Anthropic and Gemini arms map image
  parts natively both directions, `providers/gemini/completion.rs:455,1135`). A tool
  whose output is an *artifact* (`image2image <in> <out>`, `tts "…"`) is a **kaish
  builtin**: async `Tool::execute` + `register_arc` is already kaibo's own pattern
  (`Blocked`, `sandbox.rs`), paths compose with redirects/pipes/loops, and `run_kaish`
  callers get production tools with no model in the loop at all.
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
  `vision` pin in the role table, resolved caps at `kaibo://config`. What
  remains is the *consumption*: toolsets assembled from resolved caps (a vision
  model gets `view_image`; a kernel gets the `tts` builtin only when a
  tts-capable role is configured — absent, not erroring; images attached to a
  blind model fail loud). That lands with vision-in.
- **Roles outgrow explorer/synth** — SHIPPED 2026-06-11: the role table
  (explorer, synth, image, tts; `ModelRole`/`ModelSlot` in `config.rs`),
  spelled `[casts.<name>]` since the backends/casts split shipped. Media slots
  are stored but unconsumed until the production builtins land.
  `explore`/`synthesize` remain the agent costumes over `run_phase`; new
  capabilities are injected tools or builtins, never new loops.

**Sequencing:** (0) the backends/casts split — SHIPPED 2026-06-11; vision-in's
toolset assembly and the production builtins both land on resolved cast slots,
so it fronted the queue. (1) vision-in — `view_image` rig tool reading *through
the kaish VFS* (one access path; containment + ro stay structural), caller
images by path-in-scope and inline base64 both, toolset assembly reads the
synth arm's resolved caps, `consult_result` assembles `Vec<Content>`, mock
responders assert/emit image parts so media is offline-tested like everything
else. (2) First production builtin (image2image or tts) + out-dir +
ResourceLink delivery. Additive after that.

**Open design points:** session history records `[image: path, mime]` markers, not
blobs; image size caps with loud errors (no resize dep). Per-builtin timeout tuning
is its own P1 entry below — a blocker for any model-backed builtin. **Explicitly deferred:**
search/code-exec tools, file-store/context-cache plays, batch synth (its P3 entry
stands), any image-processing crate.

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

### Remove the transitional `provider` call-param alias
The backends/casts rename shipped 2026-06-11 with `#[serde(alias = "provider")]`
on the `cast` tool arg (consult/explore/synthesize), so a client still sending
`provider` selects the named cast instead of being silently dropped into the
default (`docs/casts.md` "How a call maps"). One cycle only by design: delete
the alias (and this entry) on the next release pass. `tests/cast_param.rs` pins
the alias behavior — its alias tests go with it; the duplicate-field test
becomes a plain unknown-field test (inputs are deny_unknown_fields, so a stale
`provider` turns into a loud invalid-params error, which is the desired end
state).

### Unquota'd `/` MemoryFs scratch: redirection writes unbounded bytes into RAM
**Proven live, 2026-06-10**: against the running server,
`run_kaish` with `for i in $(seq 1 200); do cat src/consult.rs >> /tmp/grow; done`
exited **0** with 17.4 MB in `/tmp/grow` in ~1s — scale the loop bound and it's
gigabytes. The per-script output cap bounds what returns to the *caller*; a
redirect lands in the `/` `MemoryFs` (`kaish-vfs/src/memory.rs`, a plain
`RwLock<HashMap>` with no size check on `write`), bounded only by the 30s exec
timeout and the call-scoped kernel lifetime. A consult's explorer holds its
kernel for the whole phase loop (50–100 turns), so one steered or pathological
investigation can drive real host memory pressure — that's the user surprise.
(The output-*spill* path is NOT part of this: `SpillMode::Memory` truncates,
bounded — initial version of this entry overstated it.)

Fix shape (settled 2026-06-10, design in kaish `docs/kaish-overlayfs.md`
"Byte accounting"): **kaish-side, always-on** — counting lives inside each
memory-resident fs (`resident_bytes()`, exact net accounting under the fs's own
lock), limiting is a shared `ByteBudget` with profile defaults riding
`KernelConfig` the way `OutputLimitConfig::mcp()` already does. This superseded
the earlier kaibo-local `QuotaFs` wrapper idea (a wrapper undercounts overlay
bases, and opt-in guards drift). **kaish landed `ByteBudget` 2026-06-10**
(re-exported from kaish-vfs), so kaibo's half is unblocked and gains urgency from
the media-spine plan (P1 above): media artifacts in scratch are megabytes by
design, not just pathological loops. 2026-06-11: waiting on a kaish 0.8.1
release (entry recorded in kaish `docs/issues.md`, including a one-line
`ByteBudget` re-export from kaish-kernel so kaibo needs no direct kaish-vfs
dep). Attach point confirmed: kaibo builds its own scratch `MemoryFs`
(`sandbox.rs:170`), so the budget goes in via `MemoryFs::with_budget` at
construction — `KernelConfig.vfs_budget_bytes` never enters the `with_backend`
path. kaibo's part once 0.8.1 lands: bump the registry dep, thread
`[sandbox].scratch_limit_bytes` → an owned labeled budget on the scratch mount
(stricter-only, like the rest of `SandboxConfig`; default generous, e.g. 64 MB
— scratch is a feature, runaway growth is the bug), and land the failing-first
test: the probe above expecting the loud ENOSPC-style refusal; prove teeth by
lifting the budget and watching it fail. Escalates to P1 (blocking) when
overlay workspaces land: long-lived kernels remove the call-lifetime bound.

---

## P3 — Infra, perf, polish

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

### Per-model prose fitting — gemini-cli probe candidates (params-first, prose-later)
Once the params seam lands, probe whether Gemini's *prose* underperforms Anthropic's
before forking any preamble. Candidates lifted from gemini-cli's codebase-investigator
(`packages/core/src/agents/codebase-investigator.ts`), the direct twin of kaibo's
`explore`, ranked by expected lift:
- **Structured report + a worked few-shot example** (`:166-189`). Its final report is
  a JSON schema (`RelevantLocations:[{FilePath,Reasoning,KeySymbols}]`) with a full
  filled-in example *in the prompt*. kaibo's explorer returns free-text "a curated
  report"; weaker models follow a *shown* shape far better than a described one. This
  touches the explorer→synth hand-off seam (`report_preamble`), so it's the highest-
  value and highest-blast-radius probe.
- **"Treat confusion as a signal to dig deeper"** (`:146`) — imperative curiosity;
  kaibo's "get more when context isn't enough", made directive. Positive framing.
- **Completeness pressure** (`:140,147`) — "don't stop at the first relevant file…
  complete and minimal set." For the cheap explorer (coverage is its job) this may be
  the right lean for Gemini.
- **Tension to test, not adopt:** gemini-cli uses `DO / DO NOT` bullets freely *with
  Gemini* and it works — against our positive-framing discipline. Hypothesis: that
  caution is **Gemma-specific, not Gemini-wide**. If Gemini tolerates prohibitions,
  its prose can be more directive than the local-Gemma profile's. Measure.
- **`<scratchpad>` mandate** (`:151-160`) — high variance: a strong scaffold for a
  less self-directed model, but pulls toward long chats, against "few high-value
  turns" and the turn cap. Probe last.
- **Debug affordance:** a `WRITE_SYSTEM_MD`-style dump of the assembled prompt to a
  file (gemini-cli's `GEMINI_WRITE_SYSTEM_MD`) — handy once prompts compose per model.

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

### OTLP / OpenTelemetry export (P4, deferred)
MCP notifications shipped first: progress (`notifications/progress`, gated on a
caller `progressToken`) and a logging bridge (`notifications/message`, `setLevel`-
tuned). Both ride seams that OTEL can reuse rather than replace:
- **Spans** — the natural trace tree is already there: a tool call → `run_phase` →
  each delegated `explore′` sweep → each `run_kaish`. The `progress::PhaseEvent`
  points (`PhaseStarted`/`SweepStarted`/`KaishRun`/…, `consult.rs`) are exactly the
  span boundaries; a `ProgressSink` impl that opens/closes spans instead of (or
  alongside) emitting MCP progress is the cleanest entry. The sink is already
  threaded everywhere via `ConsultConfig.progress`.
- **Logs** — `mcp_log::McpBridgeLayer` is one `tracing` layer feeding one channel;
  an OTLP layer is a *second* `tracing` layer in the same `main.rs` registry stack,
  no new plumbing.
- **Metrics** — turn counts, token usage (rig surfaces `Usage`), per-phase latency,
  sweep fan-out.
Deferred because it needs an exporter dep + endpoint config (a `[telemetry]` table
mirroring `[server]`) and there's no consumer wired yet. Note the **stdio-only**
invariant: an OTLP/gRPC or HTTP exporter opens an *outbound* socket — that's allowed
(kaibo must never *bind/listen*), but it's a real boundary to call out in review, and
it should be opt-in/off-by-default so a default run stays fully local. The session
context already runs an `otlp-mcp` collector, a ready sink for a first probe.

