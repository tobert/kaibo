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

Last pass: 2026-06-08 (host-env hermeticity entry retired — the kaish-side fixes it
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
source-of-truth pal configs (`provider-model-ids` memory). → Model ids now live in
profiles and are overridable per profile in `config.toml` (shipped; `docs/config.md`).
The in-sync-with-pals discipline for the built-in defaults stays regardless.

### Per-model request shaping (the `Dialect` seam): remaining knobs
The `Dialect` (`consult.rs`) now resolves request params per (kind, model) from a
profile, asked per *phase* against its own model — so a Gemini synth/explorer that
straddle the 3-line capability boundary each get the right shape. Thinking is
model-aware (Gemini 3-line → `thinkingLevel`, 2.5/3.5 → `thinkingBudget`). Remaining
knobs to land on the same seam:
- **temperature / topP — kaibo still sets neither.** gemini-cli pins its codebase
  investigator at `temperature: 0.1, topP: 0.95` (`codebase-investigator.ts:98`) —
  low temp for deterministic code reading. A natural `Dialect` knob; Gemini may
  want it more than Anthropic. Probe before defaulting it on.
- **DeepSeek V4 thinking toggle — verify, maybe an improvement.** `Dialect::thinking`
  returns `None` for DeepSeek; correct as far as rig 0.34 (its `deepseek::Client`
  parses `reasoning_content` but plumbs no request toggle). Open question: does
  DeepSeek's *current* V4 API accept a request-time thinking/reasoning param? If so,
  `additional_params` would pass it through the deepseek client. Probe the live API —
  don't assume the no-op is still complete.
- **Static budget.** `THINKING_BUDGET` (8192) and `max_tokens` (16384) are constants,
  not per-model/per-phase. Fine today; if a provider caps output below 16384 it'll
  400 (DeepSeek accepted 16384 in testing) — cap that arm rather than lowering the
  global, per the `large-token-headroom` memory. The `Dialect` is the place to make
  these per-model when one provider forces it.
- **Gemini 3.5 boundary is empirical.** The classifier (`is_gemini3_level`) flips
  only the pure `gemini-3-*` line to `thinkingLevel`; `gemini-3.5-flash` stays on
  budget because the 2026-06-06 live test confirmed budget works there. If a future
  3.5 build *rejects* budget, widen the classifier — but confirm with a live probe,
  don't guess.

All four provider paths have opt-in live tests (`tests/consult.rs`, `#[ignore]`d,
gated on a key/endpoint) and passed with thinking on — the probes above extend these.

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

### Server doesn't report which providers are usable
Keys are resolved lazily at call time, so a missing key surfaces as a mid-call
error. Validating available providers at startup (and noting them in the server
instructions) would fail faster and tell a client what it can actually use. For
the `openai` provider this would mean a startup ping of `OPENAI_BASE_URL/models`
rather than a key check. This can come along with adding an MCP resource for listing models.

---

## P4 — Eventually

### Config-overrideable system prompts (deferred until a non-Amy user asks)
Note: the in-tree `Dialect` seam (P3, "Per-model request shaping") is the *same*
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
- **Granularity** — server-wide `[prompts]` vs per-profile (prompt framing is
  model-sensitive: Gemma fixates on prohibitions where capable models don't, per the
  positive-framing discipline, so per-profile has a real case) vs both.
- **Replace scope** — override the role text only and keep injecting
  `kaish_syntax_core()` (safe: can't silently drop the grounding/exit-code contract)
  vs full-preamble raw replace vs per-field opt-in.
- **Source** — `*_file` path (keeps `config.toml` readable, prompts as real files)
  vs inline `"""…"""` vs both, mirroring `api_key_env`/`api_key_file`.

### A secrets-manager key source (deferred)
Custom credential paths shipped — a profile's `api_key_env` / `api_key_file`
override the built-in `~/.anthropic-key.txt` / `~/.deepseek-key` / `~/.gemini-api-key`
defaults (`credentials.rs`, `docs/config.md`). A secrets *manager* is still out of
scope: by design the TOML references keys, never inlines them, so "point at
`$SECRET_TOOL` output" would be a future key-source variant alongside env/file.

