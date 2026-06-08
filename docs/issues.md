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

Last pass: 2026-06-08 (offline mock harness shipped — a scripted `CompletionClient`
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

### Explorer sandbox isn't fully hermetic from the host env
Surfaced 2026-06-03 catching up to kaish's four-crate split. Several kaish builtins
read the host environment directly instead of the kernel scope: tilde expansion in
`interpreter/eval.rs` (`~`/`~/path`), `cd` with no args (`cd.rs`), and the
`~user`→`/etc/passwd` lookup (now gated behind the `host` capability, which kaibo
doesn't enable). For kaibo these are *read* disclosures, not write bypasses — `~`
expands to the host home path string, which then resolves through `/`=`MemoryFs` to
ephemeral scratch, so no real off-project file is read — but the host home *path* is
disclosed to the explorer model. The fix is kaish-side (consult the scope `HOME`,
not `std::env`) and is tracked in kaish's own `docs/issues.md` P2 hermetic pass;
recorded here so we don't lose that our sandbox's hermeticity depends on it landing.

### Provider model ids drift and live in code
`consult.rs::default_models` hardcodes the explorer/synth ids per provider; they
rot (rig 0.34's bundled `CLAUDE_*` / gemini consts are already retired — see the
`claude-3-5-haiku-latest` 404 on 2026-06-03). Keep them in sync with the
source-of-truth pal configs (`provider-model-ids` memory). → Model ids now live in
profiles and are overridable per profile in `config.toml` (shipped; `docs/config.md`).
The in-sync-with-pals discipline for the built-in defaults stays regardless.

### Thinking config tracks model generation (Gemini), and budgets are static
Thinking is on by default, both phases (`consult.rs::thinking_params`): Anthropic
`thinking`, Gemini `generationConfig.thinkingConfig`. Two watch-items:
- **Gemini 2.5 vs 3.** kaibo sends `thinkingBudget` (the 2.5 field). `gemini-3.5-flash`
  accepted it in the 2026-06-06 live test, but Gemini *3* officially uses
  `thinkingLevel` (mutually exclusive with budget). If a default id moves fully to a
  3.x line that rejects `thinkingBudget`, switch that arm to `thinkingLevel`.
- **Static budget.** `THINKING_BUDGET` (8192) and `max_tokens` (16384) are constants,
  not per-model/per-phase. Fine today; if a provider caps output below 16384 it'll
  400 (DeepSeek accepted 16384 in testing) — cap that arm rather than lowering the
  global, per the `large-token-headroom` memory.

All four provider paths now have opt-in live tests (`tests/consult.rs`,
`#[ignore]`d, gated on a key/endpoint) and passed with thinking on.

### Server doesn't report which providers are usable
Keys are resolved lazily at call time, so a missing key surfaces as a mid-call
error. Validating available providers at startup (and noting them in the server
instructions) would fail faster and tell a client what it can actually use. For
the `openai` provider this would mean a startup ping of `OPENAI_BASE_URL/models`
rather than a key check. This can come along with adding an MCP resource for listing models.

---

## P4 — Eventually

### Config-overrideable system prompts (deferred until a non-Amy user asks)
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

