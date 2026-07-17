# Persistence + CLI — living design doc

**Status: spike in flight.** This doc is working memory for the persistence/CLI
effort and gets **deleted when the work ships** (same discipline as
`docs/issues.md` entries — the PR trail and the shipped docs become the record).

## Why

Two features that complete each other:

1. **A CLI front door.** `kaibo consult "read this arch diagram and give critical
   feedback" --attach docs/arch.png` — usable by CLI-tool-first agents (pi, Codex,
   opencode, plain scripts, CI) and humans, no MCP client required. Zero resident
   token cost: `--help` is read on demand, sidestepping the 2048-char resident-pitch
   budget entirely.
2. **Session persistence.** Sessions and batch handles survive process restarts and
   are shared across front doors: start a session in Claude Code over MCP, continue
   it from the CLI. Batch handles surviving restart fixes today's orphan problem
   (provider-side batches outlive the process that submitted them).

A CLI without persistence is stateless one-shots only (fine, but half the value); a
CLI invocation is one process, so cross-invocation sessions *require* a store.
Persistence completes the loop.

We deliberately kicked persistence down the road for a long time and the design is
better for it — the read-only story stayed maximally simple while the product found
its shape. This is the point where the trade flips.

## The invariant amendment

Today's CLAUDE.md says "kaibo writes nothing, anywhere." That tightens to scope to
**what the shell can do**:

- The sandbox's four structural levers are untouched. kaish's VFS never sees the
  store; there is still no model-steerable write path and kaibo still never
  modifies the project.
- kaibo then honestly documents that it uses an XDG state dir
  (`~/.local/state/kaibo/`) for persistence that makes sessions work across use
  cases.
- The store is handler-side, at a **fixed path never controlled by a model** —
  content is model output, the path never is.
- **Failing-first test:** the store module refuses to open a path under any allowed
  tree (project roots), so the store can never be pointed into a project.
- `docs/sandbox-probes.md` gains a store-containment battery when this lands, and
  the CLAUDE.md invariant paragraph gets rewritten in the same PR.

If we aren't willing to write that paragraph, we don't write the feature.

## What the store holds

Small, and reconstructible-or-disposable (which keeps corruption stakes low — the
db is a convenience layer, never the source of truth for anything):

- **Sessions** — the `(question, answer)` turn log currently in `src/session.rs`
  (in-memory, capacity-evicted, lost on restart). Persisted: survives restarts,
  shared MCP ↔ CLI.
- **Batch handles** — provider-side batch ids + enough metadata to re-attach after
  restart (`src/batch.rs`).
- Later, maybe: job history / audit trail. Not v1.

Schema carries a version from day one (the file outlives binaries).

## Engine: Turso, with rusqlite as the boring fallback

Candidate: **Turso** — the pure-Rust SQLite rewrite (ex-Limbo), the embedded
`turso` crate, *not* libSQL and *not* the cloud SDK. Why: pure Rust fits the
static-binary invariant even better than rusqlite's bundled C (which does compile
under zigbuild — SQLite's amalgamation is plain cc, not the banned cmake/autotools
class — but zero-C-beyond-ring is a cleaner sentence). Beta-engine risk is
acceptable *because* the data is low-stakes (see above). Amy also wants to try it.

Fallback: rusqlite-bundled, if the spike fails. Costs nothing but the pure-Rust
bragging right; decades-mature file locking.

Noted and deliberately weightless in this decision: Turso compiles to WASM
(browser-kaibo someday). Fun option-preserver; the schema migrates more easily
than the reasons for an engine choice, so it carries zero weight today.

### Spike (in flight, three Opus subagents, scratchpad only)

| Leg | Question | Verdict |
|---|---|---|
| Deps / static build | Feature-flag audit; `cargo tree` free of aws-lc/openssl/cmake-C; real `cargo zigbuild` musl build verified static | **GO-WITH-CAVEATS** |
| Multi-process | **Load-bearing.** Concurrent writers from separate OS processes (long-lived MCP server + short-lived CLI invocations); open-conflict semantics; SIGKILL-mid-write recovery | **SAFE-WITH-DISCIPLINE** (one lethal footgun) |
| Store API | `SessionStore` prototype against the real async API: schema + migration, record/replay/evict, batch handles, Send futures (handler requires Send), workaround census vs rusqlite | **READY — turso beats rusqlite here** |

#### Deps / static build — GO-WITH-CAVEATS (Opus subagent, 2026-07-17)

The crate: `turso` v0.7.0 on crates.io (the ex-Limbo pure-Rust rewrite; repo
`tursodatabase/turso`). Pre-1.0, actively developed; their FAQ says it powers
production apps (Turso Cloud, Kin, Spice.ai) but recommends independent backups,
and some features are explicitly experimental. The rewrite has officially replaced
libSQL as the company's forward direction. No declared MSRV; builds warning-clean
on rustc 1.96.0.

**The hard invariant holds.** With `default-features = false, features =
["pure-rust-crypto"]`: `cargo tree -i` comes back empty for `aws-lc-sys`,
`aws-lc-rs`, `openssl-sys`, `cmake`, `ring`, `rustls` — and there is no TLS/HTTP
client transport anywhere in the tree (no reqwest/hyper/tokio-rustls/…). A real
`cargo zigbuild --release --target x86_64-unknown-linux-musl` build succeeded and
verified `statically linked` / `not a dynamic executable` (902K probe binary).
`io-uring` is correctly cfg-gated to Linux; Windows/macOS get a portable backend.

Caveats to carry:

1. **Not actually zero-C:** two `cc`-built C deps under `turso_core` — `aegis`
   (AEAD crypto; `pure-rust-crypto` does NOT remove its cc build-dep) and
   `simsimd` (SIMD kernels). Plain cc, no cmake — inside kaibo's tolerated class,
   but more C than we carry today. The "cleaner sentence than rusqlite" argument
   is dead; the choice must stand on other legs.
2. **`default-features = false` is mandatory:** defaults add `mimalloc` (another C
   build) and `fts`, inflating 189 → 316 locked crates.
3. **Sync engine is pulled unconditionally** (`turso_sync_engine` + `http` +
   serde) even with `sync` off — dead weight, but no network transport or TLS
   materializes without the `sync` feature. Keep `sync` off.
4. **Multi-process is explicitly experimental** upstream (`.tshm` sidecar for
   cross-process WAL coordination; `BEGIN CONCURRENT` MVCC) — squarely what our
   multi-process probe is testing.
5. **Big transitive surface:** 189 crates minimum (full ICU collation stack,
   prost, roaring, miette) vs. thin rusqlite.

#### Multi-process — SAFE-WITH-DISCIPLINE (Opus subagent, 2026-07-17)

Probe: `turso` v0.7.0, genuinely separate OS processes, verification by full-scan
row counts + per-writer gap/dup detection + `PRAGMA integrity_check`.

**Default mode is single-process-only.** The open takes an exclusive whole-file
lock at `build()` — a second process (even a pure reader) fails *instantly* with
`Locking error: Failed locking file … File is locked by another process`. No
busy-timeout, no retry (`busy_timeout` is irrelevant; failure precedes it).
Safe-by-refusal, zero concurrency: as-default, a second editor session's MCP
server or any CLI invocation simply couldn't open the state db.

**MP mode (`Builder::experimental_multiprocess_wal(true)`, shipped v0.6.0,
documented experimental) passed everything:**

- 4 concurrent writer processes × 500 rows: 2000/2000, zero gaps/dups/errors.
- Readers don't block writers (opened in 2ms while a writer held the db).
- **The exact MCP-server + CLI shape** (long holder with periodic writes + 12
  short open→read→write→close processes): flawless — all writes durable, shorts
  completed in 3–4ms, `max_write_lat_ms ≤ 1` under these loads.
- SIGKILL mid-write (both alone and with a surviving peer writer): clean WAL
  recovery, every acknowledged row durable, integrity ok. OS releases the lock on
  death — no stale-lock problem.

**The lethal footgun: mixed-mode opens silently lose acknowledged writes.** Docs
claim a mixed MP/non-MP open is rejected with an error; **in 0.7.0 that guard does
not fire.** Both opens succeed and operate on divergent WAL views: acknowledged
writes vanish from one or both views, `integrity_check` still says `ok`, no error
anywhere. Silent data loss — the exact failure class our "crash over corrupt"
principle exists to forbid, and the upstream guard against it is currently
unenforced.

The discipline that makes it safe (verified under it: zero loss across every
scenario):

1. **One DB-open helper in kaibo, MP flag unconditionally on, no other open path
   exists.** The CLI must not be able to accidentally open without it.
2. **Pin the turso version** — the `.db-tshm` on-disk format and API are declared
   changeable between releases (fits the kaish-pin discipline).
3. **64-bit Unix + local filesystem only** for MP mode; network mounts (NFS/CIFS)
   unsupported — validate at startup.
4. No MVCC `BEGIN CONCURRENT` alongside MP mode.
5. Belt-and-suspenders for short-lived CLI ops: open-per-operation, MP always on.

**Decisions on these findings (Amy, 2026-07-17):**

- **Windows is special-cased**, deliberately: MP mode is 64-bit-Unix-only, so on
  Windows the store runs default (single-process) mode and a concurrent second
  open fails with a **clear, loud error** — that's the requirement, not
  concurrency. Rationale: few pure-Windows users will run kaibo MCP and CLI
  simultaneously; the realistic Windows setup is kaibo + Claude Code inside WSL,
  which is the Unix path anyway.
  - **Amended (stage 5, Gemini review, flagged for Amy):** a *fatal* second-open error
    would **crash-loop** under an MCP client that auto-restarts its servers (a second
    Windows editor window). So the `SingleProcessLocked` case alone is downgraded from
    fatal to **warn-and-degrade-to-in-memory** (`main.rs`): loud on the startup log and
    surfaced as `persistence.active = false` in `kaibo://config`, never silent. Every
    other open failure stays fatal-and-loud. This is the single amendment to the agreed
    loud-fail posture — narrow by design.
- The single DB-open helper carries a **loud warning comment** explaining the
  mixed-mode silent-write-loss hazard and why the MP flag is hardwired — the
  comment is load-bearing (it's the constraint the code can't show).
- **Still leaning turso** with the above; the store-API leg's workaround census is
  the remaining input before the final call.

#### Store API — READY (Opus subagent, 2026-07-17)

Prototype: a full `SessionStore` against `turso` 0.7.0 — **14/14 tests passing,
clippy-clean** — covering behavior parity with today's in-memory `session.rs`
(LRU capacity eviction, touch-on-read/write promotion, clone-sharing, order
preservation) plus persistence-specific coverage (survives reopen, migration
idempotence, batch put/get/list/upsert, the allowed-tree path guard, 16-task
concurrent writes).

Key findings:

- **Send verdict: clean yes.** turso's `Connection`/`Database`/statement futures
  are all Send+Sync (asserted in-crate), and a compile-time `assert_send` around
  every store call passes. This is the decisive advantage over rusqlite, whose
  blocking `!Sync` connection would force a `spawn_blocking`/actor-thread layer
  (a second `KaishWorker`-style pattern) to satisfy the handler's Send-future
  requirement. Turso deletes that whole layer.
- **Data shapes confirmed from kaibo source:** a turn is lean
  `{question, answer}` (`QaTurn` — reports/tool transcripts deliberately
  ephemeral); eviction is capacity-LRU with no TTL, touched by both read and
  write; batch state today is *nothing* — the minimal re-attachable record is
  `{backend_name, provider_id}` + `created_at` + optional label, since poll/
  cancel rebuild a fresh provider client from exactly that pair.
- **Schema v1:** `STRICT` tables (`sessions`, `turns`, `batch_handles`),
  `PRAGMA user_version` forward-only migrations (probed persisting across
  reopen), and a monotone `touch_seq` LRU key so eviction is clock-free and
  deterministic in tests.
- **The design-shaping gotcha: one `Connection` forbids concurrent use** (probed:
  6/8 concurrent inserts fail on a shared connection; `Clone` shares the same
  underlying one). The working model — probed 16/16 — is connect-per-operation
  from the shared cheap-`Clone` `Database`, with a `busy_timeout`, transactions
  staying on their one local connection. A naive hold-one-connection port would
  fail intermittently under kaibo's concurrent spawned consult tasks.
- **Workarounds needed: only two, both small.** `conn.transaction()` wants
  `&mut` — use explicit `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`. And the path
  guard must canonicalize the *parent* dir (the db file may not exist yet).
  Everything else the store needs worked first try: upserts, STRICT, correlated
  subqueries, RETURNING, WAL mode, `user_version`, FK cascade (accepted, though
  the prototype deletes turns manually rather than lean on a young FK/trigger
  path).
- Confirms the deps leg independently: `default-features = false` is mandatory —
  defaults install **mimalloc as the whole binary's `#[global_allocator]`** via a
  C build, which we absolutely do not want implicitly.
- turso errors are a clean `thiserror` enum (`Busy`, `Constraint`, `Corrupt`,
  `Readonly`, …) that maps naturally onto a store error type.

### Decision: GO on turso (recommended; Amy leaning same)

All three legs converge. The purity argument died (aegis/simsimd C remains), but
the choice stands on better legs: native async with Send futures (rusqlite would
need a thread-offload layer), every needed SQL feature probed working, and the
exact MCP+CLI multi-process shape verified flawless under MP mode. The risks are
real but each has a named, tested mitigation.

**Consolidated implementation disciplines** (each traceable to a probe finding):

1. `turso = { version = "=0.7.x", default-features = false }` — exact-pin (the
   `.db-tshm` format and API are declared unstable between releases; kaish-pin
   discipline applies), and defaults off keeps mimalloc's global-allocator hijack
   and `fts` out. Keep `sync` off — no network/TLS ever materializes.
2. **One DB-open helper, no other open path.** On 64-bit Unix it hardwires
   `experimental_multiprocess_wal(true)`; the helper carries a loud, load-bearing
   warning comment: mixed MP/non-MP opens silently lose acknowledged writes and
   the documented upstream guard does not fire in 0.7.0.
3. **Windows is special-cased** (Amy's call): default single-process mode; a
   concurrent second open fails with a clear, loud error explaining it. Realistic
   Windows users run kaibo + CC in WSL, which is the Unix path.
4. Connect-per-operation from the shared `Database`; explicit `BEGIN IMMEDIATE`
   transactions; `busy_timeout` set.
5. Store lives at the XDG state path; `open` refuses any path under an allowed
   tree (canonicalizing the parent) — the failing-first containment test from the
   invariant amendment.
6. Validate local-filesystem at startup for MP mode (NFS/CIFS unsupported).
7. Store data stays reconstructible-or-disposable; the db is never the source of
   truth. Backups are the user's XDG dir; corruption = delete and start over,
   never limp.

## CLI shape (agreed sketch)

Same binary, clap subcommands. **Bare invocation stays the MCP server** so every
existing client config keeps working; `kaibo serve` becomes the explicit alias.

```
kaibo                          # MCP server on stdio (unchanged)
kaibo serve                    # same, explicit
kaibo consult "…" --attach docs/arch.png --cast gemini [--session NAME]
kaibo oneshot "…" < context.md
kaibo explore "…"
kaibo kaish -c 'grep -rn resolve_root src/'
kaibo config                   # the kaibo://config resource, printed
kaibo batch submit|get|list    # provider-side state; a natural CLI fit
```

Conventions:

- **Answer on stdout; progress and logs on stderr.** Progress beats get a terminal
  `PhaseEvent` sink (the abstraction in `src/progress.rs` already supports a new
  impl). `--json` emits the structured envelope (answer + provenance + usage).
- **Exit codes with teeth**: 0 = answer; nonzero = consultation failure /
  containment rejection, so agent callers branch without parsing prose.
- **`--help` is model-facing text.** Agents read it like a tool description; the
  "Writing for models" discipline applies verbatim, and the top of `--help` is the
  retrieval surface that decides whether a CLI agent picks kaibo up at all.
- **`--attach`** maps onto the existing attach machinery including the vision gate.
- MCP-only surface that does *not* cross over: `consult_submit`/`job_*` (a CLI
  caller backgrounds the process instead); tool-gating `--no-<tool>` flags stay
  serve-only.

Implementation seam: the MCP handlers are thin glue over the exported engine
(`consult/engine.rs` — same seam the offline tests use). The extraction is the
resolution glue currently living as `KaiboHandler` methods (`resolve_root`,
`resolve_cast`, `arm`, `house_rules`, `orientation`) into a resolver both front
doors share.

Open question, decided-by-default unless we choose otherwise: CLI containment
defaults to per-invocation cwd (same rule as the server, set per call instead of
per launch). Run from `$HOME`, that allows `$HOME` — intuitive but bigger blast
radius; a git-root heuristic is the alternative if we want one.

## Plan

1. ✅ Spike launched (three legs above).
2. Synthesize spike results here → engine decision.
3. Implementation PRs, each branch → PR → cross-family review:
   - ✅ **stage 1** — store module (`src/store.rs`, `tests/store.rs`);
   - ✅ **stage 2** — wired into config/server/CLI (see "Stage 2 — wired" below);
   - ✅ **stage 3** — the documentation half of the invariant amendment + release
     hygiene (see "Stage 3 — documented" below);
   - ⏳ resolver extraction + CLI subcommands (the `kaibo consult …` front door — this
     doc's remaining reason to live; the CLI sketch above is still the plan).
4. Melt the durable parts of this doc into the shipped docs
   (`docs/config.md` / AGENTS.md / README), then **delete it** — after the CLI ships.

## Deferred

- **Local-filesystem validation is Linux-only** (store stage 1, `src/store.rs`).
  Discipline #6 (refuse a network-mounted state dir, where MP-WAL can silently lose
  writes) is implemented as the cheapest honest version: on 64-bit Linux the store
  `statfs`es the state dir and refuses the known NFS/CIFS/SMB `f_type` magics, using
  `libc` (already an in-tree transitive dep — no new heavy dependency, gated to the
  Linux target). On macOS/Windows/other the check is a documented **no-op** — the
  `statfs` magic numbers aren't portable, and MP mode's real safety rests on the
  single-open-path discipline, not on this best-effort guard. A portable check
  (macOS `statfs.f_fstypename`, Windows `GetDriveType`/UNC detection) is deferred; if a
  macOS/WSL user ever points the state dir at a network mount, they lose the early
  refusal but keep the single-open-path protection. Revisit if it bites.
- ~~**The store does not create its parent dir.**~~ **Resolved (stage 2).**
  `SessionStore::open` now creates the state dir through `create_state_dir` — the one
  blessed `std::fs` write site, carved out of the source-level read-only guard
  (`tests/no_write_path.rs`) with the marker-line + exactly-one-site teeth. Creation
  happens only *after* the containment check, so kaibo never makes a directory inside a
  project. Every other kaibo write still goes through turso.

## Stage 2 — wired

- **Config**: a `[persistence]` stanza (`enabled` default on; `path` default
  `$XDG_STATE_HOME/kaibo/state.db`, else `~/.local/state/kaibo/state.db`), following the
  usual precedence (per-call/CLI > `KAIBO_*` env > file > built-in). CLI: `--no-persistence`,
  `--state-db <FILE>`; env: `KAIBO_NO_PERSISTENCE`, `KAIBO_STATE_DB`. Enabled with no
  resolvable path is a loud load error, never a silent in-project fallback.
- **Sessions**: a `Sessions` enum seam (`Memory` | `Persistent`) hides the backend behind
  one async `history`/`record` surface; `consult_session_turn` is backend-agnostic. `main`
  opens the durable store (fed the resolved allowed set for containment) and swaps it in;
  a failed open is a **loud startup error naming `--no-persistence`**, never a silent drop
  to memory.
- **Batch handles**: `batch_submit` records `{backend, provider_id, label}` in the store;
  a restart recovers them **on demand** via `job_list` (deduped against the live provider
  list), not by active reattachment. The provider stays the source of truth for batch
  *state*; the store is kaibo's durable memory of what it launched. Status-on-poll caching
  was judged not worth persisting (the provider is authoritative), so the v1 schema **drops
  the `status` column entirely** rather than carry dead schema — a `user_version` migration
  re-adds it trivially if status caching is ever wanted (cross-family review, finding 5).

## Stage 3 — documented

The documentation half of the invariant amendment plus release hygiene — the durable
record now lives in the shipped docs, so this section is a pointer, not a duplicate:

- **AGENTS.md invariant rewrite** (the load-bearing one): "Read-only is the product" now
  scopes read-only to what the *shell* can steer and documents the store as the one
  handler-side write (fixed XDG path, containment-guarded, the single `no_write_path`
  carve-out); a new "Persistence engine (turso)" invariant bullet folds in the disciplines
  a maintainer must not weaken (exact-pin, `default-features` off + `sync` off, hardwired
  MP-WAL + the mixed-mode silent-write-loss hazard, Windows single-process, connect-per-op).
  The release checklist gained the turso-pin + `mimalloc`-absent checks.
- **docs/config.md**: a `[persistence]` section (enabled/path, env, CLI, XDG default, what
  persists vs. never persists, escape hatch, loud-failure posture) + the layering-table rows.
- **CHANGELOG.md**: the user-facing "sessions survive restarts / batch handles re-listed"
  entry, and the read-only-toward-projects reassurance folded into the Security bullet.
- **docs/sandbox-probes.md**: Battery E (store-refuses-allowed-tree, kaish-can't-read-the-
  store — verified live, and the `no_write_path` teeth as the compile-time leg).
- **kaibo://config** now renders `[persistence]` (enabled, resolved path, active) —
  `server/config_resource.rs`, tested.

What's left to keep this doc alive: the **CLI front door** (§"CLI shape") and the resolver
extraction. When those ship, melt the durable bits into the shipped docs and delete this file.
