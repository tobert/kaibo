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

Last pass: 2026-06-03 (caught up to kaish's four-crate split — `kaish-tool-api` /
`kaish-vfs` / `kaish-tools-git` / `kaish-tools-host` — and the `Tool::execute`
`&mut dyn ToolCtx` boundary).

---

## P1 — High-leverage features & robustness

### Multi-turn sessions (the last v1 feature)
`consult` is stateless: every call re-explores from scratch. dpal keeps cap-based
LRU sessions (no TTL — Amy holds sessions open for days; eviction is
capacity-driven only) storing lean `[question, answer]` pairs, and re-explores
fresh each turn while passing prior pairs as context. We want the same: a
`session_id` arg on `consult`, an in-memory `LruCache<SessionId, Vec<(Q,A)>>`,
and the synth prompt seeded with prior turns. The exploration report stays
ephemeral (never stored — it'd be stale bloat). Not yet built.

### `MaxTurnError` is fatal — degrade gracefully instead
rig's `prompt().max_turns(n)` returns a hard error if the model doesn't conclude
within `n` turns, discarding all progress (`consult.rs::run_phase`). We bumped the
explorer cap to 50 to make it rare, but a genuinely stuck explorer still fails the
whole consult. Better: on cap-hit, force one final no-tools "write your report now
from what you've seen" turn so the synth still gets *something*. rig doesn't hand
back the partial transcript on error, so this likely needs a `PromptHook` (to
capture messages each turn) or a hand-rolled turn loop over the lower-level
completion API. Surfaced 2026-06-03: a broad multi-part question burned all 12
turns (the old cap) and failed.

### A consult has no *whole-loop* wall-clock budget
The two narrow brakes now exist — a 30s per-exec kaish timeout (`sandbox.rs`) and a
per-request LLM deadline on the rig clients (`consult.rs`, `request_timeout`). What's
still missing is a budget on the *whole* `run_phase` loop: a model that keeps making
individually-fast-enough calls but never converges (or churns through `max_turns` of
slow turns) can still run long. The per-request timeout doesn't catch that — each
call is under the deadline. A `tokio::time::timeout` around the whole
`agent.prompt(...).max_turns(...).await`, or a turn-budgeted deadline, would. Lower
priority now that the indefinite-hang case is closed; this is the long-tail-of-slow
case, not the wedged-forever case.

---

## P2 — Focused fixes & hardening

### `touch` / `mktemp` still bypass the read-only mount
Under the `localfs`-only build, the only compiled builtins that reach real state
directly are `touch` (`std::fs` mtime on existing files) and `mktemp` (real temp
files) — kaibo shadow-blocks both via `sandbox.rs::apply_denylist`. The cleaner fix
is upstream in kaish: make `touch`/`mktemp` honor the mount's read-only flag (they
already check `ctx.backend` first; the `std::fs` fallthrough is the leak), or offer
a `register_readonly_builtins` profile so kaibo can drop the shadow entirely. Much
smaller than it was — `git`/`exec`/`spawn`/`kill`/`ps` are now compile-time absent.
Tracked in the `kaish-readonly-bypass` memory.

---

## P3 — Infra, perf, polish

### Two kernel builds + two threads per consult
Each phase spawns a fresh `KaishWorker` (explorer + synth = two OS threads, two
read-only kernel builds) so the synth starts clean at the root (`consult.rs`). Fine
for now, but a busy server rebuilds kernels constantly. Consider a small worker
pool, or resetting one kernel's cwd between phases instead of rebuilding. Measure
before optimizing.

### The explorer's report is discarded at the MCP boundary
`ConsultOutput` carries both `answer` and `report`, but the server returns only the
answer text (`server.rs`). The curated report is useful for debugging the
hand-off and for "show your work" — surface it as `structured_content` or a
`kaibo://consult/last` resource, ideally behind a flag so it doesn't bloat every
client context.

### `synthesize_batch` — a tool-less, batchable synth variant (deferred from the tool-surface work)
The standalone `synthesize` we shipped is *interactive* (`{run_kaish}`) so it can
self-correct a bad cite — the panel was unanimous a tool-less synth is too weak.
The strictly tool-less, non-interactive variant is still worth having for batch
fan-out: submit → poll → read, modelled on gpal/cpal's job system. Uses the parked
`SYNTH_ONESHOT_PREAMBLE` wording in the (now-deleted) plan. It's a **per-provider
capability** like `thinking_params`: Gemini ✓, Anthropic ✓, DeepSeek?, `openai` ✗ —
`None` where unsupported. Open fork when built: many-questions/one-model vs
one-question/many-models (the diverse-opinion panel made literal).

### Retire the legacy single-phase `explorer::explore()`
`explorer.rs::explore()` is Anthropic-only and now shadowed by the multi-provider
`consult::explore` unit built on the generalized `run_phase`. Its only caller is
`tests/explorer_live.rs`. Retire it (and that test, or repoint it at
`consult::explore`) once the new path has a few real miles on it — kept for now so
there's a fallback if the recomposed path surprises us.

### Idle-timeout via streaming — investigated, deliberately NOT done (scope to openai if ever)
The per-request LLM deadline shipped: each rig client carries a `reqwest::Client`
with `.timeout(profile.request_timeout)` (default 15 min, per-profile), so a provider
that connects but never responds — the 2026-06-06 wedge, ~29 min on a local
Gemma-4-26B — surfaces an error at the deadline instead of hanging. It's a *crude*
backstop: rig's prompt loop is **non-streaming**, so the completion arrives in one
shot and a wedged server is indistinguishable from a slow one — both are one long
wait — so the deadline must sit above the slowest *legitimate* completion.

We then looked at switching to rig's streaming loop to get an *idle* timeout (no
token for N s → abort) and **decided against it wholesale.** Reasons, from reading
rig 0.34 (`agent/prompt_request/{mod,streaming}.rs`, `providers/anthropic/*`):
- **No real-time upside.** kaibo returns one final string per MCP call; partial
  answers are low-value for a grounded-citation tool. Streaming's only real gain is
  idle detection.
- **It's a quality *downgrade* for the thinking-on paths.** Non-streaming pushes the
  provider's complete assistant message back into history verbatim
  (`mod.rs:454`), so Anthropic's *signed* thinking blocks round-trip atomically. The
  streaming loop instead reassembles reasoning from unsigned deltas
  (`streaming.rs:469-657`) — the documented home of "Anthropic rejects: signature
  required" / Gemini delta-assembly / OpenAI ordering bugs. Switching would move our
  thinking-on hosted calls onto the fragile branch for no benefit.
- **It wouldn't fully fix the incident anyway.** That wedge was at *prefill* (no
  token 1). A time-to-first-token deadline still can't tell a wedge from a legit
  multi-minute big-context prefill.

If idle detection is ever worth it, scope it to **`kind = openai` only**: that's
where wedges actually happen (hosted APIs don't silently stall for half an hour), and
`thinking_params(Openai) => None` (consult.rs) means the local path uses no signed
reasoning blocks — so streaming there carries none of the reassembly fragility. Split
the budget: generous TTFT deadline + tight inter-token idle. Until then the
total-timeout backstop holds the line.

### A mock `CompletionClient` test that consult actually delegates to `explore′`
Gemini's review noted: we pin the consult *toolset wiring* offline and exercise the
*loop* live, but nothing offline proves the model's tool calls actually drive the
nested `explore′` agent and aggregate into `ConsultOutput.report`. A mock client
that forces an `explore` tool call would close that gap without live-model flakiness.

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
source-of-truth pal configs (`provider-model-ids` memory). → The "tiny config
file" half of this is now designed: model ids move into profiles
(`docs/config.md`). See the P2 config entry; the in-sync-with-pals discipline
stays regardless.

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

### A small local context window makes uncapped output acute
A local server's context window can be far smaller than the model advertises
(Gemma-4-26B reports `max_context_window` 262144; the local server box was briefly
serving it at `--ctx-size 4096` before we bumped it). The explorer dumps file
contents over up to 50 turns, so on a tight window a single wide `cat`/`rg` blows
it. kaibo now installs an 8 KB `OutputLimitConfig` via `KernelConfig::mcp()`
(`sandbox.rs`), so a single wide `cat`/`rg` can't flood — but 50 capped turns still
accumulate, and that matters more for local models than for the hosted providers.
Thinking-on makes this tighter still: reasoning now also draws on the 16384
`max_tokens`, so a local server needs a context window comfortably above input +
reasoning + answer.

### Only one `openai` endpoint can be live per process → designed
`Provider::Openai` resolves a single `OPENAI_BASE_URL` + `OPENAI_API_KEY`, so a
server instance can talk to exactly one OpenAI-compatible endpoint at a time. The
headline driver for the config work: named profiles backed by a registry, the
`provider` arg carrying the instance name. Designed in `docs/config.md`; tracked
for implementation under the P2 config entry.

### Server doesn't report which providers are usable
Keys are resolved lazily at call time, so a missing key surfaces as a mid-call
error. Validating available providers at startup (and noting them in the server
instructions) would fail faster and tell a client what it can actually use. For
the `openai` provider this would mean a startup ping of `OPENAI_BASE_URL/models`
rather than a key check.

---

## P4 — Eventually

### Credential paths are fixed → designed (paths), deferred (secrets manager)
`credentials.rs` reads `~/.anthropic-key.txt` / `~/.deepseek-key` /
`~/.gemini-api-key` (env var overrides). Custom paths are now designed: a profile's
`api_key_file` / `api_key_env` (`docs/config.md`, P2 config entry). A secrets
manager is still out of scope — by design the TOML references keys, never inlines
them, so "point at $SECRET_TOOL output" would be a future key-source variant.

### Provider-specific features are flattened
The pals (gpal/dpal/cpal) deliberately pass through provider-specific features
(deepseek `reasoning_content`, gemini search, prompt caching). kaibo's consult is
lowest-common-denominator across providers by design — the value here is the kaish
exploration, not feature pass-through. If a specific feature (e.g. prompt caching
on the synth model, which is the expensive one) proves high-value, expose it.
