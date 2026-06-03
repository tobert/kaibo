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

Last pass: 2026-06-03 (initial list, the day the two-phase consult + rmcp server
first worked end-to-end).

---

## P1 — High-leverage features & robustness

### No version control yet
The project isn't a git repo (`git rev-parse` fails as of 2026-06-03). We have a
working MCP server with 17 offline + 2 live tests — snapshot it before it grows.
First commit should include `.gitignore` (target/, key-files must NEVER be
committed), then branch for subsequent work.

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

### A consult has no overall timeout
`KaishWorker::run` (`sandbox.rs`) doesn't set a kernel exec timeout, and `consult`
has no wall-clock budget. A hung provider API call or a pathological script can
block a call indefinitely. kaish's kernel already supports per-exec timeouts
(`ExecuteOptions::with_timeout`, used by kaish-mcp) — wire one into the worker, and
consider an overall `consult` deadline. P1 because a hung MCP tool call is a bad
client experience.

---

## P2 — Focused fixes & hardening

### `run_kaish` output is uncapped — can flood the model context
`sandbox.rs::run` returns the full kernel stdout/stderr; a `cat` of a big file or a
wide `rg` dumps everything into the explorer's context, burning tokens and turns.
kaish has `OutputLimitConfig` (`KernelConfig::with_output_limit`) — wire a sane cap
into `build_readonly_kernel` and tell the model in `RunKaish`'s description that
output is truncated, so it narrows with line ranges / `head` instead of re-reading.

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

### Provider model ids drift and live in code
`consult.rs::default_models` hardcodes the explorer/synth ids per provider; they
rot (rig 0.34's bundled `CLAUDE_*` / gemini consts are already retired — see the
`claude-3-5-haiku-latest` 404 on 2026-06-03). Keep them in sync with the
source-of-truth pal configs (`provider-model-ids` memory). Consider validating ids
at startup, or a tiny config file.

### DeepSeek and Gemini paths are unverified end-to-end
Only the Anthropic path has been dogfooded. DeepSeek and Gemini are wired
(`consult.rs`, generic over rig's `CompletionClient`) and compile, but no live test
has exercised them — provider quirks (gemini tool-calling, deepseek reasoning
fields) may bite. Add opt-in live tests gated on each key, like the Anthropic one.

### Server doesn't report which providers are usable
Keys are resolved lazily at call time, so a missing key surfaces as a mid-call
error. Validating available providers at startup (and noting them in the server
instructions) would fail faster and tell a client what it can actually use.

---

## P4 — Eventually

### Credential paths are fixed
`credentials.rs` reads `~/.anthropic-key.txt` / `~/.deepseek-key` /
`~/.gemini-api-key` (env var overrides). No config for custom paths or a secrets
manager. Fine for Amy's box; revisit if kaibo runs elsewhere.

### Provider-specific features are flattened
The pals (gpal/dpal/cpal) deliberately pass through provider-specific features
(deepseek `reasoning_content`, gemini search, prompt caching). kaibo's consult is
lowest-common-denominator across providers by design — the value here is the kaish
exploration, not feature pass-through. If a specific feature (e.g. prompt caching
on the synth model, which is the expensive one) proves high-value, expose it.
