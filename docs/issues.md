# kaibo — Known Issues & Open Work

解剖（かいぼう）'s punch list. kaibo is an assistant agent for other agents — a team
of models offering *consultation* (read-only, cited codebase answers). This file is
where we record what's missing, what's fragile, and what we'd improve. Evidence-first
— name the file, the line, the *why*, and how it surfaced.

Conventions:

- **Delete entries when they ship.** Don't mark them done — remove them. The
  *reasoning* behind a ship lands in `docs/devlog.md` (dated, why-not-what); this
  list stays open-work-only so it's cheap to skim before proposing new work.
- Narrative/architecture context lives in code doc-comments and project memory
  (`kaibo-architecture`, `kaish-readonly-bypass`, `provider-model-ids`).
- Priorities: **P1** high-leverage features & robustness · **P2** focused
  fixes & hardening · **P3** infra, perf, polish · **P4** eventually.

History of shipped work moved to `docs/devlog.md` (2026-06-18). Newest entries there:
the model-facing surface pass + the full consultation ladder (arc closeout, #37–#47).

---

## P1 — High-leverage features & robustness

### Media spine — perception in, production removed

Direction settled 2026-06-28 (w/ Amy): `generate_image` removed and read-only becomes
*unconditional* — no out-dir, no handler-side write, no write path of any kind. Image
output used none of kaibo's differentiators (read-only sandbox, cross-model code
reasoning). Dropping it closes the write path that compromised the unconditional claim.
See devlog 2026-06-28.

**What stays (perception):**
- `view_image` — rig tool in the consult toolset; the only channel carrying image
  parts into model context (`ToolResultContent` image part; Anthropic + Gemini arms
  map image parts natively, `rig-core providers/*/completion.rs`). Input size cap:
  `DEFAULT_MAX_IMAGE_BYTES` (5 MiB, `consult.rs`) — promote to a `[sandbox]`/
  `[defaults]` knob if it bites. Vision-blind synths get an honest refusal up front.
- Image `attach` on `consult`/`oneshot`/`batch` — inlines as native image parts on a
  vision-capable synth. Non-file (pasted) image input deferred (YAGNI).
- Per-slot `vision` pin (`ModelCaps`, `consult.rs`) — drives the refusal gate and
  `view_image` injection. Resolved caps visible at `kaibo://config`.

**Future input modalities — audio-in / STT (perception):** speech→text *is* part of the
reasoning input stream, so it extends a slot's `ModelCaps`/`vision`-style pin — a new
capability field, not a production role. Adopt rig's `TranscriptionModel` when coverage is
sufficient (rig 0.38: openai-kind + Gemini; no Anthropic/DeepSeek), don't hand-roll a
second wire path; it rides a small second client on the same base_url/key (the openai
audio methods hang off `openai::Client`, kaibo builds `openai::CompletionsClient`). No
sound devices in scope — file-in only.

**TTS and any record/emit are output, so they don't return as roles.** TTS (text→audio) is
a render, not perception; it leaves with image gen rather than parking as a reserved seam.
If kaibo ever needs to record or emit, that's a deliberately-mediated tool — individually
gated, its own narrow surface — never a production role or a general write path. None
planned.

---

## P2 — Focused fixes & hardening

### Codebase health review (Fable-5 + Gemini Pro, dogfooded 2026-07-02)
A whole-`src/` health review by Fable-5 (via `batch_submit` + `attach`) plus a lifecycle
review by Gemini Pro (via `deliberate`). Verdict: healthy, with two swollen organs
(`server.rs`, `consult.rs`) and archaeological layers from heavy refactoring. Two findings
were fixed in the deliberate PR (the drifted `max_turns` schema defaults; a test pinning
`deliberate`'s lane-capture-before-override). The rest, roughly in value order:

- **`apply_raw_env` has no compile-time completeness forcing (marginal).** `merge_defaults`
  is exhaustive (an explicit struct literal — a new `Defaults` field fails to compile until
  wired), but `apply_raw_env` is mutation-based, so a new `KAIBO_` env var for a new field can
  be missed with no compiler nudge; `supported_kinds_list()`'s hand-maintained array is the same
  shape. A guard there is an artificial destructure — low value, deferred. (The batch-provider
  injection seam, the lane→tool partition unification, and the credentials/`batch_http_client`
  vestige cleanups from this review all shipped — PRs #41/#42/#44/#45/#47.)
- **Module splits (architecture-scale, own PRs, sequence deliberately).** `consult.rs` →
  `shaping` (`ModelShape`/`ModelCaps`/the id-classifiers/`default_models` — the provider-drift
  knowledge) + `engine` (`Arm`/`PhaseRunner`/`run_phase`/the view_image break-rewrite) +
  `prompts` (the preambles/`phase_preamble`/`consult_user_prompt`/`deliberation_prompt`).
  `server.rs` → extract the containment boundary (`resolve_root`/`contained`/attachment
  resolvers) into its own doc-headed module, plus `render_config_resource` and the job/batch
  renderers. Split the `ConsultConfig` grab-bag (its own comments admit fields ride it only
  because it's "the one bundle already threaded everywhere"; `explore`/`deliberate` fill it
  with fields their comments call inert).
- **`deliberate` dossier vs. caller timeout (Gemini).** The dossier builds synchronously, so a
  caller with a tight tool-call timeout can drop the connection before the durable handle
  returns; the job still runs. Mitigation: echo the question into `job_list` so a timed-out
  caller re-finds the handle (dovetails the "self-describing batch results" bullet under the
  batch-hardening P3 entry). A more invasive option — async dossier returning `job-N`
  immediately — trades away the batch lane's cross-restart durability; don't without the
  persistence decision.

### Bump to kaish-kernel 0.11.0 as soon as it drops
0.11.0 fixes grep BRE `\|` alternation silently matching nothing — measured live
2026-07-03 costing a deepseek explorer ~10-15 retry turns in one sweep (mitigated
meanwhile by the cheatsheet teaching `grep -rnE 'foo|bar'`, which stays valid after
the bump). Usual bump discipline applies (adapt to any API shape change, boundary
tests keep teeth).

### Upstream kaish-vfs: `LocalFs::list` hard-fails when an entry vanishes mid-walk
`kaish-vfs` `LocalFs::list` (`src/local.rs`, 0.9.0) enumerates a directory with
`read_dir`, then calls `symlink_metadata` on *each* entry and propagates any error with
`?` — so if a single sibling is unlinked between the enumeration and its per-entry stat,
the **whole** listing fails with `os error 2` (surfaced as `ls: .: not found`). This is a
real product gap kaibo inherits: a `consult` that lists a *live* repo (a running build
churning `target/`, `node_modules`, editor temp files) can spuriously fail an `ls`/`grep`/
`find`. It's also what made `omitted_path_zero_config_infers_cwd_as_default_root` flaky
(~1 in 5 full `cargo test` runs) — the test enumerated the crate root while a parallel
`cargo test` churned `target/`. The kaibo-side symptom is **fixed**: that test now reads a
single known file (`cat -n Cargo.toml`) instead of enumerating, so it does no directory
walk (`tests/containment.rs`). The real fix is upstream — `list` should skip (or tolerate)
an entry that disappears between `read_dir` and `symlink_metadata`, the way `ls(1)` does,
rather than aborting the listing. Amy co-develops kaish, so this is a direct contribution,
not a route-around; track it for the next `kaish-vfs` bump.


### Async consult (`consult_submit` + shared `get`/`cancel`/`list`) — follow-ups
The async-consult surface shipped (`src/jobs.rs` `JobStore`, `consult_submit` in
`server.rs`, the unified handle-dispatched `get`/`cancel`/`list`). Open polish:
- **Eviction silently aborts a still-running job — surface it on a burst.** The
  `JobStore` LRU aborts the least-recently-used job when a new submit pushes past
  `job_capacity` (`jobs.rs`, `Job::drop` → `abort`), intentionally (an unreachable job
  shouldn't burn tokens). But the abort is silent on the *submit* side: a caller that
  fires a burst of > `job_capacity` (default 64) `consult_submit`s in a tight cross-model
  loop has its oldest in-flight consults killed mid-investigation, and only finds out when
  `get` returns `Unknown`. Pre-existing (eviction always aborted running jobs); the 128→64
  default made it more reachable. Surfaced by the gemini-batch cross-family review
  (2026-06-29). Fix options: a `Warn` `tracing` event when eviction aborts a *running*
  (not terminal) job so a `wait`-draining caller sees it, and/or document the cap as a
  concurrency ceiling in the tool description. Backpressure (refuse a submit at capacity
  instead of evicting) is a *different* contract — don't add it without deciding. Low
  priority: 64 concurrent live consults hits provider rate limits first.
- **Completion notification is log-only (by necessity).** A finished job emits an `info`
  `tracing` event onto the MCP `notifications/message` bridge (`mcp_log`). Confirmed live:
  Claude Code does *not* surface that into the agent's loop (it's a client log/debug
  signal), and no MCP primitive wakes the caller — so it's a clue for a human/log-watching
  client, not a trigger. Don't build polling-avoidance on it. If a client that *does*
  surface server notifications to its agent shows up, this already works for it.
- **Independent gate.** The async trio shares the `--no-consult` flag; a `--no-consult-async`
  (or splitting submit vs. blocking) is the per-tool-gate ideal if anyone wants only one
  shape. Low priority — they're one capability.
- **Restart survival is intentionally out of scope.** Jobs die with the session (stdio
  lifetime = caller session), matching the subagent pattern they replace. Persisting them
  to disk is a *different product* (kaibo-as-daemon); don't add it without that decision.

### Path containment check-then-open in `resolve_root` (the attachment half is closed)
The attachment half **shipped**: `resolve_attachments` (`server.rs`) now reads *through the
read-only kaish VFS* (`worker.read_file` on a `KaishWorker` rooted at the attachment's
containing tree, one worker per distinct tree), not `std::fs::read`. The VFS resolves within
its mount and refuses to follow a symlink out of the allowed tree, so a path swapped for an
out-of-tree symlink after the friendly early check is rejected at the mount layer regardless
of timing — the check-then-open window closes structurally. Teeth: `tests/attach.rs`
proves the read goes through a worker mounted at the *correct* containing tree (a wrong
mount fails the read), atop the `mount_layer_symlink_*` battery that proves the VFS refuses
escapes. (No flaky racy TOCTOU test, per the project's no-flaky-tests stance.)

**Still open — `resolve_root`.** It also checks containment on the canonical path then hands
it to the kaish kernel as the mount root, a separate step. It's the *less-exposed* half: the
kernel opens the root as a directory fd and works through it (vs. the old one-shot
`std::fs::read` the attachment path used), and the mount itself re-resolves reads. Surfaced
by the batch `attach` cross-family review (DeepSeek, 2026-06-22), which judged the boundary
otherwise sound. The attacker model stays narrow (a self-attack: a concurrent writer to the
caller's own workspace, sub-millisecond window). Worth closing for symmetry when the model
justifies it, but lower priority than the attachment read that one-shot-followed a symlink.

### Attachment per-file streaming cap + FIFO-hang (DoS, self-attack) — count/total caps shipped
Surfaced by the Gemini Pro batch review of the VFS-read change (2026-06-23). These are
**pre-existing** and all in the self-attack model (the caller owns the request and the
workspace), so they're DoS, not confidentiality. The cheap batch-level bounds **shipped**
(`attach::check_attachment_bounds`, wired into `resolve_attachments`): a hard cap on
attachment *count* (`DEFAULT_MAX_ATTACHMENTS` = 64, checked before canonicalizing the list)
and a *cumulative* byte budget (`DEFAULT_MAX_TOTAL_BYTES` = 32 MiB, the running total
checked before each read so a batch of individually-legal files can't sum to an OOM). Two
remain, both needing more than a pre-check:
- **Per-file size cap is check-then-read, not streaming.** `resolve_attachments` checks
  `std::fs::metadata` length against `read_ceiling`, then `worker.read_file` slurps the whole
  file with **no byte cap** (`Job::Read` deliberately skips the script-output cap). A file
  swapped for a huge one *after* the metadata check is read whole into a `Vec<u8>` → OOM,
  bounded now only by the 32 MiB cumulative cap (so the blast radius shrank, but a single
  swapped file up to that budget still slurps). The real fix is a *streaming* cap in the read
  path (a `read_file` that takes a limit and aborts past `limit+1`), which needs a
  `KaishWorker`/kaish-vfs API that bounds the read. (`classify` enforces the per-encoding cap
  only *after* the full read, too late for memory.)
- **A special file (FIFO/device) swapped in can hang the read.** `is_file()` rejects a FIFO
  at check time, but a regular file swapped for a FIFO before the read blocks
  `worker.read_file` indefinitely (single worker thread; `request_timeout` covers only the
  model call, not attachment resolution). Wants an `O_NONBLOCK`/`tokio::time::timeout` bound
  on the read. Judged rare (decided 2026-06-23) — parked unless it bites.
Low priority (self-attack DoS). The streaming cap is the one with real teeth and needs the
kaish-vfs read API.

### Upstream a retry/backoff for rig's non-streaming completion path
The provider failure *policy* is now stated, audited, and documented (README FAQ +
`docs/config.md`, shipped): kaibo does **no** retry; one completion is bounded by the
backend `request_timeout`/`connect_timeout`, and a provider failure surfaces as a clean
**tool-result error** (`is_error`, `server.rs::consultation_failed`) the host can proceed
past rather than an opaque `internal_error`. What's left is the *mechanism* we chose not
to hand-roll: automatic retry/backoff for transient overload (429/503/529/reset).

The decision (with Amy, 2026-06-23) was **not** to build a retry loop inside kaibo —
that belongs in the shared HTTP layer, and hand-rolling it cuts against the anti-fork
grain (cf. the TTS "wait for rig" call). rig *already ships* `ExponentialBackoff` /
`RetryPolicy` (`rig-core 0.38 http_client/retry.rs`) but wires it **only into SSE
streaming** (`http_client/sse.rs::with_retry_policy`); the non-streaming completion path
kaibo uses gets none. So the right move is an **upstream rig contribution**: wire the
existing retry policy into the non-streaming completion call (idempotent provider calls,
a small cap, transient-status classification). If/when rig lands it, kaibo inherits
retry for free and the FAQ/`config.md` policy text updates to match. Until then, the
documented no-retry-fail-clean behavior stands and is the honest answer.

Related cleanup (DeepSeek review, 2026-06-23): the synth's kaish kernel is spawned
*lazily inside* the consult tool-loop (the toolset factory), so a kernel-build failure
lands in the same error shadow as a provider error. `consultation_failed` now classifies
it as `Internal` (named as a kaibo-side failure, not blamed on the provider) so it's no
longer *mislabelled* — but the cleaner fix is to spawn the driver's kernel in the handler
*before* the `consult()` call (the way `orientation` already does) and map a spawn failure
to `McpError::internal_error`, matching `run_kaish`. Low priority (kernel build is
in-process and reliable); the classification covers the user-facing symptom today.

## P3 — Infra, perf, polish

### Release pipeline — harden native matrix + GitHub-native signing (plan in `docs/releases.md`)
The full plan and its decisions live in **`docs/releases.md`** (living doc); this is the
tracker pointer. Direction settled 2026-06-25 (w/ Amy): **stay OSS / GitHub-native** —
keep the existing native `release.yml` matrix (native macOS, native Windows MSVC, Linux
musl via zigbuild) on GitHub-hosted runners, harden it (SHA-pin actions, `--version`
smoke per target, reproducible archives), then add the transparency payoff with free
GitHub-native tooling: `cosign` **keyless** signing + **SLSA provenance**
(`actions/attest-build-provenance`) + an **SBOM** (`syft`). **No GoReleaser as the spine**
(its cross-compile value doesn't apply — macOS can't cross from Linux given
`security-framework`, so we build native anyway); GoReleaser-OSS is an *optional later*
back-half **only if** package channels (brew/scoop/winget/nfpm) multiply. **GoReleaser Pro
and any release-as-a-service are off the table** (the latter doesn't viably exist — see the
doc's axo note). No Windows ABI change — MSVC stays, so the CLAUDE.md invariant is
untouched. The **ghcr image is a first-class, early distribution path** (multiarch, non-root
default, devcontainer-friendly; an OS-enforced containment layer under kaibo's own read-only
sandbox) — with a companion **`/reconfigure`** host-agent prompt to tame the docker/podman/
devcontainer `mcp add` config friction (kaibo advises via `kaibo://config`, the host agent
edits — kaibo can't write configs or run docker). **Going wide is gated on install ease**
(engineering PRs and even tags land first — `v0.2.0-rc.1` already proved the tag→release
leg). Sequenced PRs (1 plan doc, 2 harden matrix — both realized 2026-07-05 → **next: 3
signing/provenance/SBOM** → 4 ghcr image + container UX → 5 channels, gated on demand) in
the doc; delete this entry when the pipeline ships.

### `KaishWorker::read_file` is unbounded — stat-then-read growth race
`sandbox.rs` `Job::Read` slurps the whole file through the VFS with no size cap.
Both attachment resolvers check `metadata().len()` against their cap/budget *before*
reading, so a file that grows huge in the stat→read window gets slurped into memory
before the post-read length check demotes/refuses it (Gemini cross-family review,
2026-07-03). kaibo's stated adversary (the model) can't drive filesystem timing, so
this is robustness, not an escape — but a fast-growing log file could spike memory.
Fix shape: a capped read op on the worker (`read_file_capped(max)`) that refuses past
the cap at the VFS layer; both resolvers pass their real ceiling.

### Attach-inline follow-ons: per-call budget, observed cost
`inline_attach_budget` (2026-07-03, `[defaults]`/env) is server-wide only. Two
things to watch in real sessions before adding surface: (1) whether a **per-call
override** earns its schema slot (a caller pairing one big attach batch with a
hosted cast while the server default protects local casts would want it); (2) the
**real token cost** of inlined attachments riding every driver-loop turn on hosted
casts — the OpenRouter cost-calibration finding (cache reads 50× DeepSeek's) says
transcript-resident bytes multiply fast at 200 turns. Also: `explore` refuses image
attach because the sweep toolset has no `view_image` (`run_explore_phase` builds
`{run_kaish}` only) — if a vision-capable explorer sweep ever earns `view_image`,
revisit the refusal.

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
- **`FileRef` / Gemini File API for *batch* is bigger than "a variant beside `Image`"
  — and may be the wrong shape.** Re-scoped after checking gpal + Google's docs (2026-06-22):
  - **gpal's batch is inline-only** (`create_batch` → `InlinedRequest(contents=prompt)`,
    `/home/atobey/src/gpal/src/gpal/server.py:2394`). Its File-API uploads (`file_uris`,
    `Part.from_uri`) and **context caching** (`cached_content`) — the things that make
    gpal's `consult` fast on big/reused context — all live on the *interactive*
    `consult_gemini` path, **never** its batch. So gpal (which inspired kaibo) does *not*
    validate File-API-with-batch; don't cite it as precedent.
  - **Gemini *inlined* batch caps at ~20 MB total** (file-based JSONL batch goes to 2 GB).
    kaibo submits inlined, so 20 MB is the real ceiling (see the duplication entry below).
  - **File refs in an *inlined* batch request are unconfirmed** — Google's
    [batch-api docs](https://ai.google.dev/gemini-api/docs/batch-api) only document
    referencing uploaded files from a *file-based* JSONL batch, not inline. So a
    `FileRef` variant that just rides beside inline `Image` may not even be accepted; the
    real "files in batch" path is **adopting file-based JSONL submission** (write a JSONL,
    upload it, reference uploaded files, poll a file output) — a whole second submission
    mode, not a body-builder tweak. Park `FileRef` until that mode is on the table.

Per-provider capability, `None` where unsupported: Anthropic ✓ (shipped), Gemini ✓
(shipped — inline batch, `gemini-batch` cast synths Pro), OpenAI ✓ file-based (next),
DeepSeek ✗ (confirmed 2026-06-22 against the official API reference — no batch endpoint;
its routes are chat/completions/models only, and its cost-saving lane is off-peak
discount pricing, not a batch API; third-party batch like Novita/Together/Bedrock wraps
the model, out of reach of the keyed `deepseek` backend), local `openai` ✗.

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
- **Shared `attach` duplicates per item — bounded by the inlined-batch payload cap.**
  Attachments are shared across the batch but inlined *per item*: `anthropic_content`/
  `gemini_parts` (`batch.rs`) re-encode every attachment into every item's request, so a
  1 MiB attachment on a 20-prompt batch is ~20 MiB of body. The binding limit is the
  provider's: **Gemini inlined batch caps at ~20 MB total**
  ([batch-api docs](https://ai.google.dev/gemini-api/docs/batch-api); file-based JSONL
  batch is the 2 GB tier, which kaibo doesn't use), and Anthropic has its own per-request
  ceiling — so the wire rejects an over-cap batch before kaibo OOMs. We will **not**
  dedupe in memory (a shared `Arc<str>` would still serialize N times on the wire — the
  providers require the bytes inline per inlined request). Two real directions, in order:
  - **Near-term, cheap: a loud pre-flight guard.** Refuse before submit when the estimated
    total inlined payload (Σ prompts + attachments×items) would exceed the backend's inline
    cap, naming the cap — beats an opaque provider 400. The per-file size cap already exists;
    this is the per-batch total.
  - **Structural, later: stop holding the content in RAM / off the inline wire.** Two levers,
    both better than dedup. (a) **Context caching** (`cached_content`) — Gemini supports it
    *per request in batch*, and kaibo's attachments are *shared*, so cache the shared context
    once and have every item reference it: one upload, cache-hit pricing, no N× inline bytes.
    Near-perfect fit for our model. (b) **File-based JSONL batch** (the 2 GB tier) — spill the
    JSONL (and any uploaded file refs) to an **XDG cache dir on disk**, upload it, let kaibo
    release the in-RAM content, poll a file output. This is the "write to disk so we can let go
    of the content" direction, and it's the same machinery the `FileRef`/File-API item above
    needs — so they land together if they land. Surfaced by the holistic review (Gemini Pro,
    2026-06-22) and the docs follow-up; low priority until a big × many batch is real.

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
rediscovering it. One live one beyond the TTS coverage matrix (see the media-spine
P1 entry):

- **Gemini support is thin — text in, little else.** rig 0.38's gemini provider is
  `Completion` + `Embeddings` + `Transcription` (STT) + `ModelListing` only:
  `client.rs` declares `type AudioGeneration = Nothing` for both clients, so Gemini
  — a richly multimodal family — reaches kaibo as a text/vision-in completion backend;
  rig exposes none of its TTS, context caching, file stores, or search/code-exec
  grounding (all of which the `gpal` MCP sibling drives directly). This is why "voice
  on Gemini" is parked with TTS. Track rig's gemini coverage; adopt its traits when
  they broaden rather than hand-rolling media modalities over raw HTTP.
- **OpenRouter provider silently drops `max_tokens`.** rig 0.38.2's native
  `providers::openrouter` request struct (`openrouter/completion.rs`,
  `OpenrouterCompletionRequest`) has no `max_tokens` field and its `TryFrom` never
  reads `CompletionRequest.max_tokens`, so `AgentBuilder::max_tokens()` — kaibo's
  per-arm headroom mechanism — is a no-op there. Confirmed still present on rig
  `main` (2026-07-03), untracked upstream. Collides with the `large-token-headroom`
  doctrine (thinking eats the completion budget). Kaibo's workaround: inject
  `max_completion_tokens` (OpenRouter's preferred name; their spec deprecates
  `max_tokens`) via `additional_params`, guarded by a failing-first test. Watch
  upstream; if it accretes — with rig's other thin spots and the missing universal
  reasoning API (rig#951) — the exit is a **direct OpenRouter Rust SDK** for that
  backend (per Amy, 2026-07-03: the kaijutsu precedent — break a provider out of
  the framework when a good dedicated crate exists). Crate survey (2026-07-03):
  **`openrouter-rs`** (realmorrisliu, MIT) is the pilot candidate — active (~weekly
  releases), OpenAPI-drift CI against OpenRouter's spec, typed `ReasoningConfig` +
  `reasoning_details` incl. the Anthropic `signature` field, streaming tool-call
  accumulation, skips `: OPENROUTER PROCESSING` keepalives, `#[non_exhaustive]`
  discipline; dep tree verified ring-only (no aws-lc/openssl), though TLS is
  reqwest's default rather than a caller-pinned feature — our `src/tls.rs`
  install-default covers that. Runner-up `openrouter_api` (staler, weaker error
  typing, but caller-selectable TLS feature + MCP client). Switch triggers: rig
  lags a feature we need (reasoning-details pass-through, provider routing),
  openrouter-rs hits 1.0 / gains a second maintainer, or OpenRouter quirks get
  awkward to special-case through rig's abstractions. Adapter cost: it's a direct
  client, not a rig `CompletionModel` — the arm would hand-roll that seam.

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
- **`openai`-kind emits no thinking at all** — `ProviderKind::Openai ⇒
  ThinkingStyle::None` (`shaping.rs`), so a reasoning-capable model behind an
  OpenAI-compatible endpoint (OpenRouter fronting Claude/Gemini/DeepSeek, a hosted
  GPT, a local Qwen/GLM reasoner) is silently called with reasoning *off*. That
  inverts our doctrine (per Amy, 2026-07-03): **anything kaibo calls should have
  thinking maximized by default**, opt-*out* per slot, never silently absent. The
  fix wants model-aware shaping on the openai wire — e.g. emit `reasoning_effort`
  for OpenAI-compatible reasoners, OpenRouter's unified `reasoning` param when the
  backend is OpenRouter — landing with the first-class OpenRouter work.
- **`thinking_style` is missing from the `inert_tunables` render** (GLM review,
  2026-07-03): `kaibo://config` flags an inert `thinking_budget`/`effort`/
  `temperature` per slot (`config_resource.rs`), but a `thinking_style` set on a
  slot whose kind ignores the override (anything non-Anthropic) renders as if
  effective. Display accuracy only; add it to the inert check alongside the others.

All four provider paths have opt-in live tests (`tests/consult.rs`, `#[ignore]`d,
gated on a key/endpoint) and passed with thinking on — the probes above extend these.

### OpenRouter cost + shaping follow-ups (measured $4 GLM consult, 2026-07-03)
First live `or-glm` consult (GLM-5 explorer / GLM-5.2 synth, 8 whole files attached):
203 chat turns in 21 min, 18.2M cumulative input tokens (16.4M cache reads) for ~40K
output. Pricing was honored; the driver **never delegated a single explore′ sweep** —
every one of the 203 spans carried the consult-driver preamble (classified from the
OTLP traces' `gen_ai.system_instructions`), so the synth opened all 8 attachments and
every span itself, re-billing the growing prefix each turn at GLM's $0.18/M cache-read
rate (50× DeepSeek-pro's $0.0036/M). The delegation failure is being addressed by the
explorer-prompt/attachment work (in flight, separate session). Turn caps stay a
*runaway backstop*, not a cost throttle — the synth may burn what the question needs;
the cure for crawling is delegation.

Shipped since (2026-07-03): **prompt caching on every OpenRouter arm**
(`Arm::openrouter_completion_model` routes through rig's `with_prompt_caching`;
unit-pinned in `engine.rs`, live round-trip green — Anthropic-upstream slots now get
the `cache_control` breakpoint; implicit-caching upstreams accept and ignore it) and
**measured reasoning accounting** (`tests/consult.rs::openrouter_reasoning_accounting_live`
posts kaibo's exact shaped params with OpenRouter usage accounting on: effort=high
billed 932 reasoning tokens, the structural `effort="none"` disable billed 0).
Remaining OpenRouter-specific forks:
- **Cache the transcript, not just the system prompt.** rig's breakpoint marks only
  the system prompt, so an Anthropic-upstream slot's *growing transcript* still
  re-bills at full input price every turn — the preamble is the only cached prefix.
  Upstream rig gap; the fix wants a trailing-message breakpoint like rig's native
  Anthropic path carries.
- **Per-slot output ceilings vary by pinned slug** (`top_provider.max_completion_tokens`:
  glm-5.2 32768, kimi-k2.7-code 16384, gpt-5.5 128000) and reasoning bills into the
  same completion budget — the `[defaults]` 16384 starved a real GLM oneshot answer
  mid-sentence. Per-slot `max_tokens` already exists; the gap is doctrine: set a synth
  slot's ceiling from the catalog when pinning a slug, and watch 16384-capped reasoners
  (kimi) for starvation.
- **Provider routing variance — data policy shipped, the rest open.** One slug routes
  to multiple upstream hosts differing in cache support, quantization, pricing, and
  parameter fidelity. `provider.data_collection = "deny"` now rides every request by
  default (backend `data_collection = "allow"` is the explicit opt-in; openrouter-kind
  only, load error elsewhere). Still unexposed: `require_parameters` (a host that
  silently drops the reasoning param defeats thinking-on-by-default), `order`/`only`/
  `ignore`, and quantization filters. Belongs with the rig-OpenRouter exit-strategy
  thinking.
- **Spend visibility.** Traces already carry `gen_ai.usage.*` per turn (this incident
  was diagnosed entirely from them); consider surfacing a per-call token/cost total in
  the provenance footer or job result so the calling agent sees cost before the bill does.

### Explorer prose — residual probes (the report shape + reading strategy shipped)
The structured report sections (`SummaryOfFindings`/`RelevantLocations`/
`ExplorationTrace`), the curiosity + completeness behaviors, and the whole-first /
staged-targeted-read strategy now live in `report_preamble` (with the grep gotchas
in the shared cheatsheet; the `wc -l` pre-probe was retired 2026-07-03). Measured against a real review task,
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

### Server doesn't validate backend health at startup
Usable *casts* are now advertised — the handshake `## Casts` list and the `cast`
schema enum (`inject_cast_enum`, `server.rs`) tell a client what teams it can pass.
The remaining gap is backend *health*: keys resolve lazily at call time and an
`openai`-kind `base_url` is never pinged, so a present-but-wrong key or a down local
endpoint still surfaces as a mid-call error. A startup check (key presence + an
`openai` `base_url/models` ping) would fail faster and, under casts, report degraded
teams up front — "cast `chimera` is degraded: backend `sd` unreachable" beats a
mid-consult error. An MCP resource enumerating models could ride along.

### `config.example.toml` clarity — illustrative stanzas read as required; `gpt` dual-name
Surfaced by the cross-model review of the cast-roster change (an Anthropic `consult`
cross-checking the example against `config.rs`, plus a deepseek/gemini/anthropic
parse-back, 2026-06-29). Both are doc-clarity, not correctness — the example parses and
all three families read it right otherwise — so they were left out of the roster PR:
- **The four built-in backend stanzas are shown uncommented.** The
  `[backends.anthropic/deepseek/gemini/openai-local]` block sits live even though the
  preceding comment says they're illustrative and you only list a backend to *change*
  something. An operator scanning the file may copy all four as if required — the
  getting-started block and `[telemetry]` are commented-out for exactly this reason.
  Comment them out (or collapse to one representative stanza). Care needed: the example's
  casts resolve against these backends, so a wrong cut breaks
  `tests/config.rs::the_shipped_example_config_parses` (which is the guard that makes the
  pass safe).
- **`gpt` names both `[backends.gpt]` and `[casts.gpt]`.** The namespaces are separate so
  this is legal, but the file reserves *alias* names "at both levels" without saying a
  *primary* name may be shared across the backend/cast namespaces — a reader can't tell the
  co-naming is intentional vs. a latent collision. One clarifying clause near the
  `[casts.<name>]` header settles it.

---

## P4 — Eventually

### Recompose a single-doc kaish onboarding (if wanted)
The composed `agent_onboarding` mental-model view is no longer produced anywhere — the
surface pass (PR #37) deleted `kaibo_instructions`/`kaish_reference` along with the resident
kaish reference. Nothing is lost for a caller today: the `kaibo://kaish/*` topic resources
(`syntax`, `builtins`, `vfs`, `scatter`, `sandbox`) cover the reference piecemeal and
`run_kaish`'s description carries the operating contract. If a single-doc kaish onboarding is
ever wanted, recompose `Recipe::agent_onboarding()` behind a `kaibo://kaish/onboarding` resource.

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
