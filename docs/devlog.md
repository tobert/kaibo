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

## 2026-07-18 — Gemini's effort lever was silently disconnected

Investigating batch truncation (#79, already fixed) turned up a *separate* correctness
bug: the Gemini thinking-knob classifier, `is_gemini3_level`, matched only the bare
`gemini-3` / `gemini-3-*` form. But every Gemini id kaibo actually ships is a 3.x-line
model the substring test missed — the `-latest` aliases the built-in casts pin
(`gemini-flash-lite-latest` explorer, `gemini-pro-latest` batch synth) carry no
`gemini-3` at all, and the dotted previews they resolve to (`gemini-3.1-pro-preview`)
put a dot after the 3. All of them fell to the `GeminiBudget` arm, which emits a fixed
`thinkingBudget` and **drops the per-role `effort`**. A user setting `effort = "low"`/
`"high"` on a Gemini synth was quietly ignored — no crash, since both knobs are accepted
on the wire, just the wrong one sent.

The prior code deliberately kept `gemini-3.5-flash` on budget, citing a 2026-06-06 live
test that confirmed budget *works* there. The flaw in that reasoning: "budget works"
never established "level fails" — and if both work, `thinkingLevel` is strictly better
because it carries the effort lever. Rather than re-probe, we went to Google's current
[thinking doc](https://ai.google.dev/gemini-api/docs/thinking): it lists `thinkingLevel`
(`minimal|low|medium|high`) across the whole 3-line — 3-pro, 3-flash, **3.5-flash**,
3.1-pro — and doesn't mention `thinkingBudget` at all; that's the retired 2.5-era knob.

So the fix isn't a wider classifier — it's a *collapse*. Amy's call: kaibo targets the
Gemini 3-line and newer, and we say so. `ProviderKind::Gemini` always resolves to
`GeminiLevel`; `is_gemini3_level` and the whole `GeminiBudget` `ThinkingStyle` variant
are gone (dead surface once nothing produces it — there's no `thinking_style` override
for Gemini). A pre-3.x id now emits a level and fails loud at the provider, the honest
"unsupported" signal, over silently mis-shaping. TDD: a failing test pinning the
configured ids' effort → `thinkingLevel` came first (RED on `gemini-flash-lite-latest`),
then the collapse turned it green. Dependent tests that used 2.5 ids as fixtures moved
to 3.x; the engine per-arm straddle test — which had leaned on the now-defunct
*intra-Gemini* budget/level split — was re-aimed at a stronger cross-*provider* straddle
(Anthropic-adaptive synth vs Gemini-level explorer, structurally disjoint param trees).

## 2026-07-05 — the build bootstrap: never-fired workflow → verified first release, in a day

The release plan (PR #25, `docs/releases.md`) had sat unmerged since 2026-06-25 with its
central artifact — `release.yml` — never once executed. Amy's call: review the plan, update
it, then *realize* it as quick small PRs, Sonnet subagents on the toil, short bootstrapping
prose. The day's shape validated two of the plan's own bets and overturned two predictions:

- **"Fire it first" beat speculative hardening.** The baseline `workflow_dispatch` went
  all-five-legs green on the first run ever — the pre-flight cross-family review's
  high-confidence prediction (macOS `sha256sum` missing) was wrong (runners ship coreutils),
  so the dial-in list became warnings-driven, not failure-driven. Measure, then fix.
- **The one prediction that fired, fired on *us*.** The `ref_name`-slash footgun (flagged
  latent, slated for later) broke slice (a)'s own validation dispatch from `ci/…` — the
  validation path *is* a slash-bearing branch. Moved into the slice that hit it.
- **Every slice validated live before review** (dispatch from the branch), and cross-family
  review earned its keep twice: DeepSeek's 7z-entry-order finding became a stated
  single-file boundary comment, and its taiki-e/dtolnay pin semantics all checked out
  against independently-verified digests.
- **The rc smoke tag closed the loop.** `v0.2.0-rc.1` exists to prove the never-run publish
  leg *before* signing lands, so the real v0.2.0 ships born signed (plan PR 3). Proven the
  way a user would: release download → checksum → fully static → `kaibo 0.2.0-rc.1`.
- **CI arrived the same day** (#64) because the release bones were already right — same
  pins, same voice, plus the TLS invariant finally getting automated teeth (a `cargo tree`
  tripwire asserting on error text; exit codes proved unreliable for absent packages).

Ships: #60 pins+permissions, #61 arm leg+smoke, #62 reproducible archives, #63 rc bump,
#64 first CI, tag `v0.2.0-rc.1`. PRs 3–5 of the plan (signing/provenance/SBOM, ghcr image,
channels) remain, sequenced in `docs/releases.md`.

## 2026-07-04 — `job_wait` parks-and-coalesces instead of returning on the first line

Running the kaish 0.11.0 pre-release review as one background consult, Amy parked with
`job_wait(timeout_secs=300, level="info")` and watched ~130 back-to-back calls return in
*seconds* each — never near the 300s window. The tell: if it actually blocked to timeout,
130×5min is absurd; being able to fire 130 in quick succession meant it was returning on
the first event, not the window.

Contributing factor, found in the code: `level` was doing two jobs. `wait_level_floor`
became `wait_drain_with`'s `return_floor`, and the drain loop returned the moment it
collected *any* record at that floor. At `level:"info"` that's the first `running kaish: …`
line — so observability ("watch the narrative") and "park until something real happens"
were mutually exclusive: `warn` blocked nicely but returned no story, `info` told the story
but degenerated into a poll storm. The tool description already promised the *right*
behavior ("returns as soon as a job finishes or fails, or on a clean timeout") — the code
was what had drifted.

Amy's call (two forks, both settled before code): **reinterpret `level`** as the
observability sample floor only — never the return trigger — and **wake on any Warn+**
(a job finished/failed *or* a real mid-flight warning; narrative below Warn never cuts the
park short). So `wait_drain_with` grew a third floor: `drain` (what to consume + stream to
the human), `sample` (what rides back in the tail), and `wake` (what ends the block early,
always Warn). The returned sample also went from first-`limit` (head) to a sliding
last-`limit` **tail**, capped at Warn so the terminal ping is always present even if a
caller asks for a higher floor. The default (`warn`) is unchanged — only sub-warn levels
stop poll-storming. This matters because the live progress stream reaches the *human* only
(see `[[mcp-notification-channels]]`): the returned tail is the model's sole window into
the narrative, so coalesce-and-return-the-tail is the actual mechanism, not a nicety.

TDD, with teeth proven by reverting the loop to the old behavior and watching two of the
three new `mcp_log` tests fail: `sub_wake_…parks_to_timeout` (wake decoupled from sample)
and `over_limit_…newest_tail` (tail, not head); `a_warn_wakes_…` is a behavior lock that
passes both ways. Model-facing text re-read holistically: the `level` schema doc (was
literally "Lowest level to return"), the tool description, and the `kaibo://tools` resource
all now say *`level` sizes the sample, never the timing — for more frequent check-ins, pass
a shorter `timeout_secs`*.

## 2026-07-03 — attach means the model sees the bytes (inline + sweep directives)

Amy asked how attachments reach explorers, and the honest answer — "they don't" —
exposed the gap: `consult` attach only *named* files in the driver prompt, the
delegated `explore′` sweep never heard about them at all, and the kaish output cap
(64 KB) meant "read it in full with `cat -n`" was physically impossible in one pass
for a big file. The caller's strongest signal ("these files are central") was the one
input we delivered weakest.

The reframe, per Amy: **attach means the model sees the bytes, one semantic across
tools.** How each tool delivers it:

- **`consult`** — text attachments inline whole into the driver prompt inside the
  shared `<file>` wrapper, now numbered `cat -n` style (`attach.rs::number_lines`) so
  citations against inlined content are as exact as shell reads (raw un-numbered bytes
  invite guessed line numbers — the numbering IS the citation contract). Numbering
  applies to every inline site (oneshot/batch too) — one wrapper, one form.
- **Inline budget** — `[defaults] inline_attach_budget` (256 KiB default, env/file
  overridable, `0` legal = inline nothing): inlined bytes ride *every* turn of the
  driver loop, so the budget bounds resident prompt cost, and it's the escape hatch
  for small-context local casts (gemma4's 4K ctx). A file past the remaining budget
  *demotes* — named with its size under a command-voice directive (Amy's wording call:
  "Read each one WHOLE", not "read early") with the sed-span paging idiom so "whole"
  survives the output cap. Demotion is loud, never a drop; caller order, greedy.
- **Explorer sweeps** — `explore′` preambles (and the top-level `explore` tool, which
  grew an `attach` arg) get the same command-voice read-WHOLE directive listing every
  text attachment: a sweep is a fresh agent that saw neither the driver prompt nor the
  inlined bytes, so without this a driver that delegates early sends sweeps blind to
  the flagged files. Directives, not bytes, on purpose: the explorer keeps agency over
  *when* the read happens (and pays it only in sweeps that run); images stay out
  (shell can't read them; `explore` refuses image attach outright).
- **Security posture preserved** — inlined consult bytes are read through the
  read-only kaish VFS (`resolve_consult_attachments` now mirrors
  `resolve_attachments`' TOCTOU-safe mount read); the 16-byte `std::fs` sniff remains
  only as a routing *hint* for files never inlined. Non-UTF-8 non-image attach is now
  refused loudly on consult too (it could neither inline nor be `cat`'d — naming it
  would burn a turn on a dead end).

Offline coverage: numbered-wrapper unit tests, budget partition (inline/demote/order,
budget-0), scripted-loop tests pinning the directive into the `explore′` sweep preamble
and `explore_with`, config-ladder tests for the knob. 21 test binaries green.

Live watch coda (same day): Amy watched a consult's progress notifications on the
fresh binary and the explorer was still chatty — grep-grazing and small spans. The
guidance was the culprit: "a *short* file: read it WHOLE" makes the model classify
before daring a whole read, the prescribed spans (400 lines) were far smaller than
the cap affords, and `grep -B4 -A8` was offered as a way to *understand* rather
than to locate. First instinct was to also raise the output cap to 256 KiB;
Amy's context-budget arithmetic killed that: measured on our own tree kaibo Rust
runs **2.79 bytes/token** (cpal count over `server/mod.rs`), so a maxed 256 KiB
read ≈ ~94K tokens — a third-plus of a common 250K *explorer* window, riding every
subsequent turn of the sweep. (Synths are 1M-class and get attachments inlined
under the separate `inline_attach_budget`; the cap protects the explorer.) The
landed design is hers, staged: cap stays 64 KiB (~23K tokens; fits nearly every
real file whole), whole-first wording everywhere ("Read files WHOLE… nearly every
source file fits in one look"), grep reframed as the locator, `wc -l` pre-probe
dropped — and a truncated giant is *informative*: exit 3 hands back head+tail, and
the guidance stages the rest as targeted reads (`grep -n SYMBOL FILE`, then a
~1,200-line span around it) instead of a mechanical end-to-end walk. Caller-flagged
attachments keep their read-it-ALL directive; that cost is deliberate.

Cross-family review (Gemini Pro batch + DeepSeek agent, whole files, no diff) folded
in before merge: path escaping extended to the demotion/image/sweep directive lists (a
filename can legally hold `\n` and would have forged list entries — both reviewers);
`deliberate` gained `attach` (both flagged the one-semantic-everywhere gap — dossier
directives, so content reaches the offline synth through the dossier); the `explore`
tool description now names `attach`; plus an end-to-end tempdir pipeline test and a
non-circular escape assertion. Gemini's "critical wrapper breakout via `<\/file>`" was
a misread — an attacker's literal `<\/file>` is already the escaped form and can't
read as a bare terminator; the "exactly one bare `</file>`" invariant holds (now
pinned by an impl-independent test). Its unbounded `read_file` stat→read growth race
is real but preexisting and kaish-side — logged in issues.md.

## 2026-07-03 — whole-call wall-clock deadline (a consult can't hang overnight)

A live consult (cast `lemonade`, a local server since decommissioned) parked a Claude
Code session ~17 hours overnight until Amy interrupted it. Forensics on the still-alive
process: kaibo idle, no in-flight work, all MCP socket queues empty — the tool call had
*reached* kaibo (two `kaish` worker threads spawned the same second the transcript shows
`consult` dispatched) and simply never returned. The `--cast lemonade` came from a stale
`~/.claude.json` pointing at `:13305`, a server Amy retired 2026-06-26; that's the
trigger (fixed out-of-tree), and the reason the real defect hid so long.

The defect: the per-request `request_timeout` (down in reqwest, injected via rig) was the
*only* brake. The 2026-06-06 fix (`tests/llm_timeout.rs`) proved it catches "no bytes
ever", but not every wedge — a stalled response *body* across rig's split send/bytes read,
or a pooled keep-alive to a wedged server — and with 900s configured it still ran 17h, so
it plainly didn't fire. kaibo trusted the transport for liveness and had no backstop of
its own. Fix: a kaibo-owned `tokio::time` ceiling — `call_deadline` on the base
`PhaseContext` rung (config `call_deadline_secs` / `KAIBO_CALL_DEADLINE_SECS`, default 1h),
wrapping the model work via a new `with_call_deadline`. Transport-agnostic, so it fires
whatever the wedge shape.

Where the ceiling *applies* took Amy's push-back to get right, twice. It bounds the
interactive **loop** tools — `consult`/`explore`/`oneshot` and the async `consult_submit`
(same multi-completion loop). `deliberate`'s direct lane is one long *in-process*
completion kaibo holds, so it must be bounded too — but binding it to the same
`call_deadline` would force a slow local `deliberate` to raise `call_deadline`, re-opening
the interactive-hang window the fix just closed. So the bound keys off the *shape* of the
work: deliberate-direct is exactly ONE completion, bounded by its synth backend's own
`request_timeout` (+ a 60s margin) — the value the operator already tunes for a slow
model, auto-scaling without touching the interactive ceiling. The batch lane holds no
in-process wait at all (the deliberation runs on the provider's queue), so `call_deadline`
structurally can't and doesn't bound it. Default set *above* the largest per-request
timeout (even a 30-min local model) so it never cuts a legitimately slow completion — it
turns "overnight" into "you'd notice within the hour". A deadline abort classifies as a
transient/retryable condition (retry / raise the knob / proceed), not a kaibo bug. Tests:
offline consult/explore/deliberate paths aborting a wedged (`hang_model`) scripted backend,
a real black-hole-socket integration test proving `call_deadline` dominates a *generous*
300s wire timeout, a pure test pinning that deliberate-direct's deadline tracks
`request_timeout` and outlasts `call_deadline`, and the classification guidance.

## 2026-07-02 — positive-framing sweep across the model-facing prompts

Follow-on to the explorer wide-span change (PR #48). Amy's principle: if something's
worth a negative example, double up on complementary *positive* ones instead — a "not X"
names the very pathway we're trying to suppress (the CLAUDE.md positive-framing rule). We
swept the sibling model-facing blocks for the same "not X / rather than X / beats X"
phrasing and, where they carried explorer-like reading guidance, extended the wide-span
framing:

- **`consult_preamble`** (the driver reads code directly too): "pull a whole file rather
  than a narrow slice … what a surgical read would miss" → read generously in wide passes,
  a short file whole or a big one in a few hundred-line spans (`sed -n '1,400p'`, then
  `'401,800p'`).
- **`KAISH_SANDBOX_ADDENDUM`** (the shared cheatsheet every tool-driving preamble embeds)
  and **`kaibo_sandbox_doc`** (the `kaibo://kaish/sandbox` resource): same wide-span,
  positive rewrite; the old narrow `sed -n '40,80p'` example became a wide `'1,400p'` walk.
- **`oneshot_preamble`** / **`deliberation_prompt`**: dropped "rather than guessing" — the
  wanted behavior (name the gap so the caller can supply it / reason under a stated
  assumption) is already said positively; `batch_preamble` had already been fixed this way,
  these were the stragglers.

No behavior measured (unlike #48's A/B), but the consult driver and cheatsheet now carry
the same validated wide-span guidance, so a consult over big files should benefit from the
same mechanism. The kaish `\|` grep habit is still routed upstream (kaish#60), untouched here.

## 2026-07-02 — explorer reads big files in wide spans (A/B-validated)

The explorer was chatty — many tiny reads. We instrumented it (PR #46's
`kaish.exit_code`/`output_bytes` spans), traced two real deepseek sweeps, and found the
cause: **the model slices big files into ~15-line reads by choice, not truncation** (the
64 KiB cap never binds — 0–1 exit-3, every read well under it). `report_preamble`'s
"most files are short / narrow-slice-after-truncation" gave a file too big to read whole
no strategy, so the model defaulted to timid slicing.

Fix (this change): give a big file a first-class **wide-span** strategy — `cat -n FILE |
sed -n '1,400p'`, then `'401,800p'`, a few hundred lines a look. A/B (treatment binary
swapped live, same questions, OTLP-measured): a broad sweep dropped **74 → 46** calls,
reading `consult.rs` in **~13 wide spans (~100 lines each) instead of ~22 tiny ones**.
Consistent across both test questions.

Deliberately *not* included, though we tried them in the A/B: a `grep -E`-for-alternation
steer and a `for f in …; do cat -n` batch idiom. The A/B showed the grep steer is
**unreliable** — the model's GNU `grep 'a\|b'` reflex is too strong for one prose line
(one run adopted `-E`, another ignored it and got *worse*, 12 failed greps). The real fix
is upstream: **kaish#60** (support BRE `\|`), which also unlocks the multi-file batching
the model reflexively attempts (`grep 'a\|b' f1 f2`). So this PR ships only the validated
lever and leaves the grep line untouched. Ruled out entirely: a `slurp` tool / bigger read
budget — the cap doesn't bind, so it would solve a problem we don't have.

## 2026-07-02 — run_kaish spans carry the script's exit code + size

Chasing a real question — the explorer is chatty, doing many small reads where one
wide `cat -n` should do — we reached for a trace to tell *forced* narrow reads (a
whole-file read truncated at the 64 KiB output cap) from *chosen* ones (the model
slicing when it didn't need to). The trace couldn't answer it: `RunKaish::call`
returns `Ok(format_output(&out))` for **every** kaish exit code (a non-zero *script*
exit is normal output for the model, not a tool failure), so the `tool` span's
`outcome` reads `ok` for a truncated read, a timeout, a blocked op — all of them. The
exit code and size were buried in the output text, invisible to telemetry.

Fixed at the source: `RunKaish::call` now calls `tool_span::record_kaish_result(out.code,
out.stdout.len())`, tagging the enclosing `tool` span with `kaish.exit_code` and
`kaish.output_bytes`. Everything needed was already in the `KaishOutput` snapshot
(`code` + `stdout`) — no kaish-kernel change. The field *names* live in `tool_span.rs`
beside their `field::Empty` declaration on the span (a caller can't silently mistype
one); every non-kaish tool leaves them empty, so they don't export. We left the
pre-truncation original size out of scope — kaish trails it in the output text, but
`exit_code == 3` plus the follow-up reads in the trace already answer the read-size
question, and pulling it out cleanly would be a kaish-dependent nice-to-have.

TDD with teeth: a unit test drives the recorder through the real `Traced` wrapper, and
an end-to-end test runs a real `RunKaish` over a worker with a 64-byte cap reading a
file that overflows it, asserting `kaish.exit_code = 3` lands on the span. Both fail
when the recorder is neutered (verified). This is groundwork for the explorer-prompt
A/Bs (for-loop multi-file reads, batched `cat|sed`) — measure the contributing factor
before changing the prompt.

## 2026-07-01 — the model-facing surface pass + the full consultation ladder (arc closeout)

A two-arc effort (07-01 → 07-02, PRs #37–#47) planned in `docs/desc-and-schema-tuning.md`
(now retired — the durable *rules* live in AGENTS.md's "Writing for models" and
`docs/casts.md`; this is the *story*). It began by sitting in the caller's seat: we ran
kaibo under Claude Code (07-01) and looked at what actually reaches the calling model.

**The finding that drove it.** Claude Code truncates the MCP `instructions` **and each
tool `description` at exactly 2048 characters, per server, hardcoded, silent** (binary
v2.1.198, constant `hQ=2048`; no config knob, debug-log only). kaibo's `## Scope` section
and kaish resource pointers were past the wall — the calling model never saw them. Two more
host facts: MCP tool schemas default to **names-only** in deferral hosts (Claude Code,
Codex) and there the `instructions` double as the tool-search **retrieval index** (the
opening words decide whether our tools get *found*); Claude Desktop **never** shows
`instructions` to the model at all, so each description must stand alone. These are now the
budget rules in AGENTS.md.

**Arc 1 — the holistic surface PR (#37).** One PR reworking the whole surface, reviewed
whole because it's one composition read by one audience: handshake restructure (scope
*above* the kaish wall, kaish reference off the resident text, a 2048-char budget test +
Scope-ordering test written failing-first), lead rewrite, `get`/`list`/`wait`/`cancel` →
`job_*`, every description rewritten to the drafted targets, `consult` pinned resident via
`_meta["anthropic/alwaysLoad"]`, the cast roster single-homed in the `cast` enum, rustdoc
cross-refs stripped from shipped schemas. Cross-family review (DeepSeek via kaibo's own
`consult`, holistic — no diff handed to it) caught three real ones before merge: stray old
tool names in `batch.rs`/`main.rs` narration, a dead `kaibo_instructions`/`kaish_reference`
pair left behind, and an *unconfigured* handshake at 2451 chars (over the wall on a fresh
install) — compressed to 1970, with a test to pin it.

**Arc 2 — the ladder (#38–#40).** Per-slot lane reshape first (#38): `Lane` (`batch |
direct`) moved onto `ModelSlot`, `Cast::batch` gone, `batch = true` normalized to sugar —
which let a cast pair an interactive explorer with an offline synth (the `deliberate`
shape). Then `explore` (#39): the evidence-gathering half of `consult` as its own tool, one
explorer phase returning the cited report *verbatim*, sharing a `run_explore_phase` seam
with the nested `explore′`. Then `deliberate` (#40): `explore → offline synth`, two lanes —
provider batch (durable `backend/provider-id` handle) or local direct (session-scoped
`job-N`). Its review caught a real gating gap (a 3rd async-handle producer that
`--no-batch`/`--no-consult` could strand). The ladder now ships end to end:
`run_kaish → explore → consult → deliberate`.

**Health follow-ups (#41–#47).** We dogfooded a whole-`src/` health review — Fable-5 via
`batch_submit` + `attach`, Gemini Pro via `deliberate`. Verdict: healthy, two swollen organs
(`server.rs`, `consult.rs`), archaeological layers. Shipped from it: a `BatchProviderFactory`
seam so the batch handlers are testable offline (#41, the thinnest coverage vs. blast
radius); the lane→tool partition single-sourced through `CAST_ENUM_RULES` with a drift guard
binding the shipped enum to each tool's gate (#42); `explore` widened to any cast with an
explorer via `cast_can_explore` (#44); dead pre-backend OpenAI-key helpers removed (#45 —
**verify-first paid off**: Fable's "vestiges" list overstated the dead code, so `load` and
the `ProviderKind::FromStr` alias table stayed); `run_kaish` spans tagged with exit code +
output size (#46); and the TLS client build folded to one site, `crate::tls::https_client`
(#47), re-proving the C-free invariant. Left open in `issues.md`: the architecture-scale
`consult.rs`/`server.rs`/`ConsultConfig` module splits, the `deliberate`-dossier-vs-caller-
timeout mitigation, and the marginal `apply_raw_env` completeness guard.

## 2026-06-29 — kaish-kernel 0.10.0

Bumped the published dep `0.9.0 → 0.10.0`. API-compatible this time — no kaibo call
sites changed and the builtin/tool set is identical. Under the hood kaish gained bignum
arithmetic (`num-bigint`) and swapped in a new regex engine (`regex-bites`); both are
shell-engine internals that surface to a user only through `run_kaish`/`consult`, so
there's no kaibo behavior we authored to record in the changelog (and 0.2.0 is the first
tracked release anyway — no baseline to diff against). Offline suite green (462), TLS
tree still C-free (`aws-lc-rs`/`aws-lc-sys`/`openssl-sys` all absent), boundary tests
still have teeth. Per the kaish-bump discipline: adapt to the new shape, don't pin
around it — nothing to adapt here.

## 2026-06-29 — A "flaky test" was a real inherited robustness bug

`omitted_path_zero_config_infers_cwd_as_default_root` failed ~1 in 5 full `cargo test`
runs and the tracker had filed it twice (P2 + P4) as a "cwd race" — the hypothesis being
that two cwd-reading containment tests race on the process-global `current_dir()`. That
hypothesis was wrong, so I reproduced it (~10% under load) and instrumented the failure
instead of trusting the note. The diagnostics killed the cwd theory outright: on a failing
run `proc_cwd`, its canonical form, and `handler.default_root()` were *all* the correct
crate root. The real symptom was `ls: .: not found: No such file or directory (os error
2)` — a **real** ENOENT from a real syscall, not a logic error.

Tracing it down through `KaishWorker` → `LocalBackend` → the `kaish-vfs` `LocalFs` mount
landed on the contributing factor: `LocalFs::list` (`kaish-vfs/src/local.rs`, 0.9.0)
`read_dir`s a directory, then calls `symlink_metadata` on *each* entry and `?`-propagates
any error — so if a single sibling is unlinked between the enumeration and its per-entry
stat, the **entire** listing fails. The test enumerated the *live* crate root while a
parallel `cargo test` churned `target/`; one vanished artifact sank the whole `ls`. Only
this test asserted `ls` *content* (others check exit status), so only it surfaced the
flake.

Contributing-factors, not root-cause: (a) the test enumerated a live, churning directory
to prove cwd inference; (b) `kaish-vfs` `list` hard-fails on a vanished entry instead of
skipping it the way `ls(1)` does. Fixed (a) in scope — the test now reads one known file
(`cat -n Cargo.toml`, no directory walk) and asserts `name = "kaibo"`, still proving the
omitted path resolved to *this* crate; 120/120 clean after. Recorded (b) as an upstream
`kaish-vfs` item in `issues.md`: it's a genuine product gap (a `consult` listing a live
repo with a running build / `node_modules` churn can spuriously fail), and since Amy
co-develops kaish it's a direct contribution, not a route-around. Collapsed the duplicate
tracker entries into that one upstream note.

## 2026-06-29 — Async-job polish: a `job_capacity` knob and `get` that shows the work

Two `docs/issues.md` follow-ups from the async-consult surface, landed together because
both live on `JobStore`/`consult_submit`.

**`job_capacity` is its own knob.** Jobs had been borrowing `defaults.session_capacity`
for their LRU cap — fine for a trial, but a held job result (a full answer + optional
explorer report) is heavier than a session's lean Q&A pair, so the honest cap is smaller.
Now `[defaults] job_capacity` / `KAIBO_JOB_CAPACITY`, default **64** (vs sessions' 128),
plumbed the same way as `session_capacity` (struct + default + raw + merge + env + a
zero-is-rejected guard) and surfaced in `kaibo://config`. Independent tests pin the
default, the file/env override, and the loud zero-rejection.

**`get` now shows the work.** The issue note said `consult_submit` ran on a `NullSink` so
`get` could only report "running, Ns" — but that premise had already gone stale: the async
path moved to `TracingSink`, and a `wait` tool + notification ring shipped, so the live
sweep/turn narrative *was* reachable, just through `wait`, not `get`. Confirmed that with
Amy and took the residual: a caller who polls with `get` (the natural verb) still saw no
motion. Fix is a small `ProgressLog` **decorator** in `progress.rs` — it records the latest
beat's one-liner + a beat count while teeing every event on to the inner `TracingSink`, so
the `wait`/`mcp_log` stream is untouched and the job *also* remembers its last beat. The
job holds the same `Arc<ProgressLog>` the running consult emits through; `get`/`list` echo
`currently: exploring … (step N)` on a still-running job (the step count advances even when
two polls catch the same kind of beat), and a finished snapshot drops it — a Done job
carries its answer, not a stale "currently" line. The decorator shape means no second
emit path and the existing `wait` view is byte-for-byte unchanged.

The model-driven progress events were already in `PhaseEvent`; this just adds a second,
pull-side consumer beside the push-side `TracingSink`. Tests: `ProgressLog` starts empty /
remembers the latest / tees to its inner sink (a counting sink proves forwarding), and a
`JobStore` test drives a beat through the shared log and asserts the running snapshot
echoes it while a finished one doesn't.

## 2026-06-28 — Dropping image generation: kaibo perceives and reasons, it doesn't render

A design conversation with Amy reversed the `generate_image` direction. The clean
principle that fell out: **input modalities fuse into reasoning, output modalities are
stateless transforms.** When a model takes an image (audio, later) as *input*, those
bits land in the same context it reasons over — perception and cognition share one pass.
That's `view_image` and image attachments on the consult tools, and it's core to what
kaibo is. Image *generation* has no such fusion: prompt in, pixels out, no reasoning, no
session — and `generate_image` as built used none of kaibo's differentiators (the
read-only kaish sandbox, the cross-model code reasoning). It's a transform that happened
to live in the same binary, a `gpal`/dedicated-MCP call away anyhow, and it billed every
caller's context for its tool description every session.

So the *capability class* itself — "run a model, hand a binary artifact back" — is the
part that didn't fit. `generate_image` wasn't an unlucky first example; it was
representative (TTS is the same shape). We're removing it and the whole binary-artifact
**write path** it forced: the out-dir, `out_dir_readable`, the world-shared-temp fallback,
the read-only out-dir mount, the allowed-set widening. The payoff is the invariant:
**"read-only is the product" stops being a fenced exception and becomes unconditional** —
kaibo writes *nothing*, anywhere, through kaish or handler-side.

What we *kept open*, on Amy's framing: read-only is the floor, but a write isn't forbidden
forever — it's just never a *general* path. If kaibo ever needs to record or emit
something, that's a **specific, individually-mediated tool** with its own narrow surface,
granted on purpose so we can mediate it. "We can give them as many tools as we want, and
it's a safer, clearer surface for writes." None exists today. And future *input*
modalities (audio-in) extend a slot's `ModelCaps`/`vision` pin — not a new production role.
The cast role-model loses `image`/`tts` accordingly; `explorer`/`synth` + the `vision`
capability stay.

**Docs first, then the code — both in this PR.** The doc rewrite (AGENTS.md, README,
`docs/config.md`, `docs/casts.md`, `config.example.toml`, sandbox-probes, CHANGELOG)
recorded the direction first; the deletion followed in the same branch. Gone:
`generate_image.rs`, `image_gen.rs` (the `ImageGen` seam), the out-dir machinery
(`out_dir`/`out_dir_readable` config + parse + defaults + the `DefaultOutDir`
shared-temp classifier, the sandbox mount, the allowed-set widening, the consult-attach
`OutDirAttach` branches), the `ModelRole::Image`/`Tts` production roles, `image_capable_casts`,
the `--out-dir`/`--no-out-dir-read`/`--no-generate-image` flags, and rig-core's `image`
feature — ~1,200 lines net. The role model is now just `explorer`/`synth` + the `vision`
pin. **The teeth:** `tests/no_write_path.rs` scans production `src/` and fails on any
filesystem-mutating call outside `#[cfg(test)]` — it would have flagged the old
`write_artifact` (`create_dir_all` + `fs::write`), and read-only is now *unconditional*.
The 2026-06-26 out-dir entry and the 2026-06-13 `generate_image` entry below are now
history — the feature they describe has been removed.

## 2026-06-26 — Artifact out-dir: `generate_image` writes a file, hands back the path

The narrow *specific-tool* write the RW-mount deferral pointed at, now built.
`generate_image` no longer streams a base64 blob inline; it writes the image to a
kaibo-owned **out-dir** and returns the absolute path. The caller's context pays nothing
for a multi-MiB picture until it chooses to open the file — and a generated image is an
artifact worth keeping, so a durable home beats an inline blob that evaporates.

**The shape, and what we rejected getting there (conversation w/ Amy).**
- **Path, not `ResourceLink`.** We weighed returning an MCP `ResourceLink` (+ the
  bytes served lazily as a binary resource on `resources/read`). Dropped it: a
  spec-compliant client resolves a `ResourceLink` by reading it back *from the server*,
  so kaibo would grow a `file://` `resources/read` channel that bypasses project
  containment — a genuinely new read surface needing its own hard-look review — for no
  win over a plain path the calling agent opens with its own tools. "Just files,
  wherever `--out-dir` points" (Amy) is the whole feature.
- **Handler-side write, never kaish.** The artifact is written with `std::fs` in the
  tool handler (`generate_image::write_artifact`). kaish is untouched — all four
  read-only levers in `sandbox.rs` still hold, kaish can't write anywhere. The
  "read-only is the product" invariant didn't move; it gained one sentence naming the
  capability-tool write path.
- **Out-dir is read-back-mounted.** The dir is mounted **read-only** into every kaish
  kernel (`build_readonly_kernel_and_vfs`, the existing `VfsRouter` multi-mount — *not*
  the deferred general-RW machinery) so a later `consult`/`run_kaish` can read a
  generated artifact back. This is forward substrate for image2image; nothing needs it
  yet (the agent opens the path itself).
- **Default `$XDG_CACHE_HOME/kaibo`, and *why* it's kaibo-owned, not `/tmp`.** Amy first
  said `/tmp`; we landed on the XDG cache because the read-back mount widens read-*scope*
  to whatever the out-dir is — and a consult can ship what kaish reads to a remote model.
  Mounting bare `/tmp` would expose every other process's temp files to a consult.
  A kaibo-owned subdir keeps the read-scope to *our own* generated artifacts. The
  containment test pins exactly this: the out-dir is readable, a sibling is **not**.
  Durable (a cache, not auto-cleared) by choice — "change it later if users ask" (Amy);
  no cleanup built.

**The inline cap is gone.** `GENERATE_IMAGE_MAX_BYTES` existed only to bound base64 in
the caller's context; with path delivery there's nothing to bound, so a large image is
just a large file. The over-cap error path (and its test) retired with it.

**Teeth.** `tests/sandbox.rs` `out_dir_*` battery: artifact readable back, out-dir
**read-only** to kaish (mount it `LocalFs::new` and the write-denial assert fails), and
a sibling of the out-dir **not** exposed. `tests/config.rs` pins the `out_dir`
precedence (default < file < env < CLI) and the sandbox mirror. `tests/generate_image`
covers the write (bytes verbatim, ext from MIME, unique names, lazy dir creation) and
path-only delivery. `docs/sandbox-probes.md` gained Battery E for the live audit.

The read-scope boundary moving — even narrowly to a kaibo-owned cache — is the part of
this that deserved the careful look; it's why this PR wants a cross-family review on the
mount + the out-dir read path specifically.

**Follow-up from the reviews — out-dir is fully *readable*, with an off switch (Amy).**
The Gemini review flagged that an artifact couldn't be re-`attach`ed to a follow-up
consult/oneshot (the out-dir wasn't in the containment `allowed_set`, and consult-attach
required a subpath of the project root). Amy's call: make the out-dir readable rather than
leave the wart — *"add out-dir to allowed_set for read and have a way to disable that … I
don't think this would be surprising."* So one knob, `out_dir_readable` (default true,
`--no-out-dir-read` / `KAIBO_NO_OUT_DIR_READABLE` / `[server] out_dir_readable`), now gates
**both** sides of out-dir readability together — the kaish read-back mount *and* the
allowed-set membership — so "readable" stays one concept. When on: the out-dir joins
`allowed_set` (so oneshot/batch `attach` and per-call `path` accept it), and
`resolve_consult_attachments` accepts an out-dir file as an *absolute* path (the consult
shell mounts root + out-dir, the only two trees it can read; an in-root file stays
root-relative, an out-dir file is absolute, anything else refused). When off: no mount, not
in the allowed-set — kaibo writes artifacts and returns paths but never reads them back.
Surfaced in `config.example.toml`, `kaibo://config`, and `docs/config.md` so it's
adjustable and visible, per Amy. The boundary still only ever widens to the kaibo-owned
out-dir (the consult-attach test pins that a *sibling* of the out-dir is still refused).

**Safe default for the no-XDG/no-HOME case — a real exfil hole closed (Gemini + Amy).**
A scenario-driven Gemini Pro batch (attachments via kaibo, asked to *play through* the
code paths) confirmed Amy's instinct that the `temp_dir()/kaibo` fallback was a genuine
hole, not just untidy: in a stripped container with no `$XDG_CACHE_HOME`/`$HOME`, kaibo
landed in a *world-shared* temp and auto-mounted it read-only into kaish. An attacker who
pre-plants `<tmp>/kaibo` as a symlink to `/etc` or `/home/<someone>` would have kaibo
canonicalize it, mount the **target** read-only, add it to `allowed_set`, and a consult
could exfiltrate it — the `out_dir = "/"` guard doesn't catch a symlink to a non-`/` tree.
Fix (Amy's "no XDG ⇒ no automatic permission", Gemini's refined option b): classify the
default out-dir (`DefaultOutDir::Cache` vs `SharedTemp` in `config.rs`), and when it's the
shared-temp fallback default **read-back off** — kaibo still *writes* the artifact and
returns the path (no frustrating hard failure in a container; the agent opens it with its
own tools), but won't auto-mount a world-shared temp. An explicit `out_dir` is trusted
(read-back on); an explicit `out_dir_readable` always wins. A startup warning in `main.rs`
and primed `configure`-prompt prose (step 5, `server.rs`) tell the agent how to restore
read-back by naming a kaibo-owned dir. The remaining write-side symlink (artifact dropped
at a symlink's target) is a low-severity nuisance — unique names prevent overwrite —
tracked in `docs/issues.md`, not the critical read path. Tested via injected-env
classification (`default_out_dir_from`, all three branches incl. the empty-`$VAR` guard).

## 2026-06-26 — Stayed read-only: retired per-builtin timeouts *and* deferred general RW mounts

Two linked decisions in one session, one posture: **kaibo stays read-only as the product;
any future write is a specific capability tool writing its own artifact, never a general
mechanism and never `consult`.** Both retire/defer machinery built for a more general
write surface we've decided we don't want.

**Retired the per-builtin-timeout work (a seam for a customer we won't build).** Amy
questioned the premise of the P1 "Per-builtin timeouts" entry directly: *if we never
make model calls from inside kaish, do we still need to mess with timeouts?* We don't —
so the entry is gone, not deferred-with-a-note.

The whole entry existed for **one** scenario: a kaish builtin making a minutes-long model
call *under the script clock*, which the 30s `KAISH_EXEC_TIMEOUT` watchdog would kill
mid-flight. That's the only way a legitimately-slow operation lands under that clock. The
30s budget governs *scripts* (runaway `grep`/`find`/loops) and for that it's correct.

`generate_image` already proves the alternative is the real shape: a capability is a
**handler-side MCP tool**, its provider call bounded by the backend `request_timeout`
(rig's HTTP timeout), never touching the script watchdog. TTS/image2image-as-MCP-tools
are identical. So the dependency chain is per-builtin timeout ⟵ in-kernel model builtins
⟵ shell *composition* of model ops — and composition was always the deferred "later
concern," with `generate_image` deliberately chosen as a direct tool because it was
simpler. With composition looking like an idea that doesn't play out, the `ctx.patient`
seam has no consumer. Building it now would be a mechanism for a customer we've decided
not to build.

What we kept: the *revival condition*, recorded in the media-spine entry. If a capability
ever genuinely needs shell composition (a generated artifact fed to another model op
within one `run_kaish` script), the slow op is back under the script clock and the problem
returns — but the upstream seam already exists (`ctx.patient(budget) -> PatientGuard`,
kaish 0.8.2+), so it's a pickup, not a research task. The `KAISH_EXEC_TIMEOUT` doc-comment
in `sandbox.rs` already states the 30s-bounds-a-runaway-script rationale correctly and
needed no change.

**Deferred the general RW mounts; reversed "consult gets RW."** The day-earlier RW-mounts
design (2026-06-25) gave kaibo a *general* writable-mount surface (`rw_paths`) wired
*uniformly* into every kernel — explicitly including `consult` — as the substrate for
future RW tasks. Amy's call: *"defer this a while and stay read-only by having only
specific tool accesses write to the underlying fs."* That flips the key sub-bullet. The
general mount is parked; when a capability needs to deliver an artifact larger than the
inline cap, it gets a *narrow, tool-specific* out-dir and returns a `ResourceLink` —
decided per tool when first needed, not a broad `rw_paths` surface. Knock-on: the
danger-surfacing design is moot (no broad mount to grade), and the consult-can-write
prompt-injection exposure evaporates (consult stays read-only). The carefully-reasoned
general-RW sub-bullets stay in `issues.md` under a "superseded, kept for reference" banner
— if we ever revisit a general mount the canonicalize-before-route safety work is still
the right shape; we're declining the *cost/exposure*, not faulting the design.

Why both, why now: the two share a root. The per-builtin timeout only mattered for model
calls *inside kaish*; the general RW mount's headline justification was being the substrate
for those same in-kernel tasks (image-gen → image2image → …). Pull on "capabilities are
handler-side MCP tools, kaibo's kernels stay read-only" and both fall out together. The
read-only invariant doesn't relocate after all — it holds.

Process note: rode one small docs-only branch even though it's deletion/deferral — the
visible trail is the point (`docs/issues.md` is open-work-only; the *why* of a not-doing
lands here). Heads-up for a future reader: PR #26 ("Lighten the RW-mount danger policy")
merged 2026-06-25 tuned the danger-surfacing for a feature now deferred — that tuning is
dormant, not live behavior.

## 2026-06-24 — Async consult (`consult_submit`), unified collect verbs, and a 24h `list` trim

The seed was a self-observation: Claude (the caller) was spawning throwaway sub-agents
*just* to hold a blocking `consult` open in the background. That's the missing primitive
hand-rolled out of the only async construct an agent has — and a bad version of it (a full
agent context for zero reasoning, lossy relay of the answer, no progress visibility). So:
make `consult` submittable.

**Async needs in-process state, not disk persistence.** The instinct was "we'll need
persistence at that point." We didn't. `batch` gets async *for free* because the work lives
at the provider and the handle *is* the provider's id — but a consult's tool loop runs
*inside* kaibo, so an async consult has to hold the live loop in-process. That's
statefulness, not durability. For a stdio server whose lifetime *is* the caller's session,
disk buys nothing — a restart has no context to resume into. So `JobStore` (`src/jobs.rs`)
is a clone of the `SessionStore` pattern: `Arc<Mutex<LruCache>>`, capacity-LRU, no TTL,
diskless. A job dies with the session, exactly as the sub-agent it replaces did.

**The scratch-collision worry was unfounded.** Pre-work assumption: concurrent consults
share the `/` `MemoryFs` scratch. They don't — `consult.rs` already spawns a fresh
`KaishWorker` (own kernel + scratch) per call, so the isolation was always per-call. No
isolation work was needed; read-only project + per-call scratch already pay for it.

**Collapsed the management verbs by handle shape, not an op-enum.** Rather than
`consult_get`/`consult_cancel` paralleling `batch_get`/`batch_cancel`/`batch_list`, the
collect verbs unified into one `get`/`cancel`/`list` that route on the handle (`/` ⇒ batch
`backend/id`, else consult `job-N`). Rejected the one-tool-with-`op`-enum collapse:
tool-calling models dispatch reliably by *tool*, far less by an enum arg, and the per-op
required-args differ (get/cancel need a handle, list doesn't). Net 6→5 async tools, one
mental model ("submit returns a handle; get/cancel/list manage handles"), and consult
gained a `list` it never had. The verbs gate on `batch || consult` (present while either
is on), and `list`'s batch section degrades to a *note* when no batch backend is
configured rather than sinking the consult-jobs section — a local-only setup is the common
case there.

**`list` recency filter: status, reframed as time.** Amy flagged the batch list dumping
42 entries (token waste). The literal "drop old stuff" reading barely helps — almost all
42 were *recent* but *terminal* (done). The waste is finished batches, not old ones. But a
24h window captures it anyway: the offline SLA is ≤24h, so a batch older than a day is
done and still collectible by its handle. So default-trim to the last 24h (Amy's call),
`all: true` for orphan archaeology, and an undateable batch is *kept* not hidden (losing
sight of a batch is worse than a line). **Brought in `chrono`** for the RFC3339 parse
rather than hand-rolling calendar math — Amy's preference, and it's already in the tree
(kaish-kernel / rmcp / schemars), used parse-only so its `clock` feature stays off and no
new dep (or aws-lc) rides in.

**Completion notification is a clue, not a trigger — proven live.** A finished job emits an
`info` `tracing` event that rides the existing tracing→MCP `notifications/message` bridge.
But we tested it: the notification does *not* surface into Claude Code's agent loop (it
goes to the client's log/debug view). So it's advisory — useful for a human watching logs
or a client that surfaces them; the calling agent still closes the loop by remembering its
handle and polling. No MCP primitive wakes the agent, and we didn't pretend otherwise.

## 2026-06-22 — `$VAR` expansion in `root` / `allow_paths`, for portable scratch reads

Amy asked the real question behind "can a user easily allow kaibo to read `/tmp`?": the
mechanism existed (`--allow-path` / `KAIBO_ALLOW_PATHS` / `[server] allow_paths`), but the
*path* you had to write was host-specific — a literal `/tmp` is wrong on macOS (per-user
`/var/folders/...`) and on sandboxed Linux (`$XDG_RUNTIME_DIR`). kaibo already followed
XDG for the config file but had no XDG/POSIX awareness in the read boundary. The fix is to
let `root` / `allow_paths` expand `$VAR` / `${VAR}` (and the leading `~` they already did),
so `allow_paths = ["$TMPDIR"]` resolves per machine.

**Chose general `$VAR` expansion over a curated temp-var set or an `--allow-tmp` knob.**
Amy's call: the general form is least-surprising to anyone who's used a shell, and it
costs no more than the narrow one — `$TMPDIR`/`$XDG_RUNTIME_DIR`/`$HOME` all just fall out,
plus whatever else a user names. A one-off `--allow-tmp` switch would have been more magic
for less reach.

**Expansion order is env-first, then tilde.** Two reasons it's not the other way: the
tilde step keeps operating on an `OsString` `$HOME` (a non-UTF-8 home survives, as it did
before), and a variable whose *value* carries a leading `~` (`MYDIR=~/data`) still expands.
A single pass only — a variable's value is never re-scanned for `$`, so there's no
expansion-injection surprise from environment contents.

**Undefined / empty / non-UTF-8 variable is a loud load error, not a passthrough.** A
silent gap (`$TMPDR` typo → empty segment → a path that canonicalizes *somewhere*) is
exactly the data-corruption failure mode the house rule rejects; refuse at startup. Two
cross-family reviews (DeepSeek V4 Pro via `consult`, Gemini 3.1 Pro via the batch lane)
hardened this: Gemini caught that `undefined → error` does *not* cover a **set-but-empty**
variable — `$EMPTY/scratch` collapses to `/scratch`, `$EMPTY/` to `/` (the whole
filesystem) — a real silent boundary-widening, now refused in `resolve_var` (extracted as a
pure seam so the rejection is unit-tested without mutating process env). Both flagged the
`$HOME`-non-UTF-8 error conflating "set but not UTF-8" with "not set"; now distinguished.
And on the bare-`$` question the two reviewers split — DeepSeek called shell-style
passthrough fine, Gemini argued a stray `$` in a boundary path masks a typo and there's no
way to write a literal `$`. Took Gemini's stricter line as the better fit for "silent
fallbacks are a mistake": `$$` now escapes a literal `$`, and any other stray `$` is an
error.

**Scoped to the boundary knobs (`root` / `allow_paths`), not every path field.** `[context]`
`user_files` and the key-file paths still tilde-only; extending them is consistency work,
not the temp-read goal, and it widens the fallible surface — logged as a follow-up rather
than smuggled in. The rest of the surface Amy asked for shipped together: the `configure`
prompt gained an opt-in read-scope step, the containment-error message names the portable
`$TMPDIR` form, and `docs/config.md` documents it — so a user meets the idiom wherever they
hit the boundary.

## 2026-06-22 — `oneshot` grows `attach` (the interactive twin of batch attach)

Amy's framing: `oneshot` should be as close to `batch` as the API split allows — "call
Claude Opus in a single shot with some files, no tools, no wait." So `oneshot` now takes
the same `attach: [paths]`, reusing the whole containment + classify seam batch built
(`resolve_attachments`, `classify`, the shared `contained`/`containment_error`). The only
thing that differs is the wire, and that's the decision this entry records.

**Chose "grow oneshot against rig" over "a batch-shaped seam with an interactive impl."**
oneshot already runs through rig's completion path, so the cheap, honest factoring is to
map each `Attachment` onto rig's own message parts rather than invent a second submit
abstraction. Text folds into the prompt string via `attach::with_text_context` (the same
`<file>` wrapper batch emits — `Attachment::wrapped_text` is the one source of truth, so
batch and oneshot wrap identically). Images become `UserContent::image_base64` parts on
the single user turn. The rejected alternative — making the `BatchProvider` seam carry an
"interactive" variant that calls rig — is heavier and only pays off if a third lane shows
up; noted in issues.md, not built.

**The one structural change: `run_phase` takes an `extra_parts: Vec<UserContent>`.** The
shared loop built its initial message as `Message::user(string)` — text only. oneshot's
images need to ride on that *initial* turn beside the text, so the parameter threads
through `Arm::run` → `PhaseRunner` → `run_phase`, which now builds a multi-part user
message when parts are present. Every other phase (consult, explore) passes `Vec::new()`
and gets byte-for-byte the old `Message::user(prompt)` — pinned by a test
(`oneshot_without_attachments_is_the_bare_prompt`) so the no-attachment path can't drift.
This is the "image is the new plumbing" cost the batch entry predicted; it lives in the
shared loop, not a oneshot fork, so it's one initial-message builder for everyone.

**DRY'd the vision gate.** batch and oneshot refuse an image to a vision-blind model
identically, so the check moved to one `KaiboHandler::gate_image_attachments` both call —
and being a plain method (no `Peer`), it's unit-testable directly, which the oneshot
handler (needs a `Peer` to drive end-to-end) otherwise isn't. rig gives us
`ImageMediaType::from_mime_type` to turn our sniffed mime into rig's media type, so the
image round-trips cleanly. Offline test drives the real loop and asserts the inbound
request carries both the `<file>`-wrapped text and a structured image part.

## 2026-06-22 — two honesty/discoverability fixes: image cast enum + inert-tunable flag

Both came off the P3 list; both are about not lying to the caller by omission.

**`generate_image` now advertises its cast enum.** The consultation tools stamp the
usable-cast roster onto their `cast` param as a JSON-Schema enum so an agent reads the
menu off the schema, but image gen didn't — its menu is a *different* filter, since it
selects the `image` slot, not explorer/synth. Wrote `Config::image_capable_casts`
(casts with an `image` slot on an openai backend whose key resolves — the only kind rig
0.38 drives for images) and generalized `inject_cast_enum` to take the tool list, so
the same advisory-enum machinery now serves both groups. Deliberately a *separate*
filter from `usable_casts`: a cast with a stranded explorer key but a working image
slot belongs on the image menu and not the consult menu, and vice versa.

**`kaibo://config` flags inert per-slot tunables.** A slot whose resolved `ModelShape`
has no sink for a knob still load-validated it and rendered it as if effective — a
`thinking_budget` on an effort-driven model (Gemini 3-line, Anthropic adaptive) or the
toggle-less openai path, an `effort` on a budget model, a `temperature` an Anthropic
slot drops under thinking. Added three predicates to `ModelShape`
(`sinks_thinking_budget` / `sinks_effort` / `sinks_sampling`) as the single source of
truth — they mirror `write_thinking`/`to_params` exactly — and the render now lists any
set-but-unsent knob as `inert_tunables`. Chose render-time flagging over a load-time
warning: the same knob is live or inert depending on the model id it lands on, so the
resolved view is where the truth is, and a `[defaults]` value that's inert for one slot
but live for another shouldn't warn globally. Didn't *reject* inert knobs — they're
valid config that simply doesn't apply to this model, and a slot may keep one for when
its model id changes; surfacing beats forbidding.

## 2026-06-22 — kill the `tool_span.rs` capture flake (root cause, not symptom)

The two span-capturing tests in `tool_span.rs` failed ~5% of full-suite runs (the
issue guessed ~25%), never in isolation — a test failing when we *didn't* make a
mistake, the opposite of teeth. The issue's two guesses (serialize the pair; race on
a "process-global span store") were both off the real mark. We traced it instead of
guessing, and the contributing factors turned out to be one specific tracing
fast-path:

`info_span!("tool")` registers its callsite **lazily**, on first hit. While tracing's
`has_just_one` optimization holds — true whenever ≤1 dispatcher is registered, which
is *exactly* our case since each test installs a single subscriber via `set_default` —
that first registration computes the callsite's `Interest` from **the registering
thread's current default**, not from the test's installed subscriber. This binary is
full of `consult`-loop tests that run the real tool loop with *no* subscriber. When
one of them won the race to first-touch the `tool` callsite during a capture test's
window, it cached `Interest::never()` against `NoSubscriber`, gating the span off
process-wide → an empty capture. So serializing the two capture tests can't fix it:
the poisoning thread is a *third* test with no subscriber at all.

The fix matches the cause. We hold one extra registered dispatcher alive for the whole
process (`force_multi_dispatcher`, a leaked `Dispatch::new(registry())`), forcing
`has_just_one` false so every callsite registration consults the *registered-dispatcher
set* — which contains a span-enabling registry — regardless of which thread triggers
it. It is never any thread's default, so it receives no events; it exists only to keep
the registration path honest. We kept a serialization `Mutex` over the two tests too
(belt and suspenders against their `set_default` installs/teardowns interleaving), and
moved them off `#[tokio::test]` onto a private current-thread runtime via `block_on`,
so the ordering guard isn't held across an `.await` and the future polls on the same
thread whose `set_default` is in scope. Proven: 0 failures in 150 full-suite runs
(was ~5%), and both tests still fail under deliberate mutation of the span name and
the outcome field. Rejected the symptom-level "just serialize" because it left the
~1% consult-thread poisoning window open — measured, didn't assume.

## 2026-06-22 — batch: `attach` — inline workspace files as context

Amy wanted "let's get gemini pro batch to give us feedback on README.md" to mean *attach
the file*, not paste it: kaibo reads README (or a `git diff > x.diff`) and inlines it, so
the bytes never round-trip through the calling agent's context. That framing — "if the
provider has attachments cool, but I don't care, just inline it for us" — settled the whole
design. **Caller-side inlining, not provider attachment APIs.**

The decisions, and what we rejected:

- **Same surface, two encodings.** One `attach: [paths]` param, *shared* across every
  prompt in the batch (the way `system` is shared) — because the stated cases are one
  prompt + one file, and "N questions about the same README" is the natural multi-prompt
  shape. Per-prompt attachments (prompts-as-objects) were rejected as YAGNI. Text splices
  in as `<file path="…">…</file>`; an **image** can't splice into a string, so it becomes a
  structured base64 part the provider carries natively (Anthropic `image` block / Gemini
  `inlineData`). That's the real cost: the two body builders had to stop emitting a bare
  string and emit **structured parts** — built now (even though text is the common case) so
  image wasn't a later re-architecture. Anthropic content stays a plain string when there
  are no attachments, preserving the existing wire shape.

- **Reusing the boundary, not a parallel one.** "Restricted to workspace and worktrees like
  everything else" became *literally* the same code: factored `resolve_root`'s containment
  into `contained` + `containment_error`, and `resolve_attachments` calls them — so an
  attachment can't read outside the workspace any more than `run_kaish` can. This is a
  *second* fs-read path outside the kaish VFS, but it doesn't weaken read-only (we only
  read), and it's the same pattern `[context]`/house-rules already use (server-side Rust
  reads a file into a preamble). Proved the teeth: defeating `contained` makes the
  outside-path and symlink-escape tests read outside bytes and fail.

- **Images gated on real vision capability.** An image to a vision-blind synth would be
  silently ignored or error upstream, so we refuse honestly up front via the existing
  `ModelCaps` classifier — and we moved that gate (and attachment resolution) *before*
  building the network client, so a vision misconfig is reported with **no key and no
  network**. That also made the gate offline-testable (pin a slot `vision = false`, attach
  an image, assert the refusal).

- **Loud failures, no silent truncation.** A path outside the set, a directory, an
  oversized file, or a binary that's neither UTF-8 nor a known image is a clear refusal —
  the UTF-8/image sniff (shared with `view_image`) is the branch point, not just a guard.

**Gemini File API: designed for, deliberately not built.** It's real (upload → `file_uri`
→ `fileData` part) and could ride in batch, but it's *Gemini-only* and the use cases are
text/inline-image, which both providers carry natively with no upload lifecycle. So it's a
reserved typed `FileRef` variant (see `issues.md`), earning its keep only for
genuinely-too-big-to-inline media. **`oneshot` deferred** the same way: same win, but it
runs through rig (not the hand-rolled batch HTTP), so its image path needs rig multimodal
messages — a later, separate piece.

## 2026-06-22 — batch: Gemini provider (second backend behind the seam)

Added the second `BatchProvider` impl — Gemini inline batch — so the offline lane reaches
the model Amy actually wants there: Gemini **Pro**. Pro is near-unusable interactively
(slow), but batch is exactly where that latency is free, so it's the natural home for it.
Shipped a ready-made `gemini-batch` **cast** (synth → `gemini-pro-latest`) rather than
adding a `batch` role slot to the schema — today's cast machinery already expresses "a
team whose synth is Pro," and the per-call `model`/`backend` override covers the rest. The
deferred `batch`-role-slot alternative stays parked; it earns its keep only if one cast
needing *both* a cheap interactive synth and a Pro batch synth becomes common.

The seam paid off: the trait, the stateless `backend/provider-id` handle, the
ScriptedBatch offline harness, and the four verbs were all reused unchanged. What's
genuinely Gemini-shaped lives in pure, offline-tested functions, and dispatch moved into
`batch.rs` (`submitter`/`poller`/`batch_supported`) so `server.rs` no longer hardcodes a
provider — it asks the module, and an unsupported kind is refused in one place.

**Probed the wire shape, didn't guess** (the CLAUDE.md rule, and the `issues.md` entry
demanded it). The live probe earned its keep — Gemini's batch differs from Anthropic's in
ways no amount of reading would have settled:

- **Body shape can't be shared.** Gemini nests *both* the completion budget
  (`maxOutputTokens`) and the thinking block under one `generationConfig`, where Anthropic
  carries `max_tokens` top-level. So `gemini_batch_body` folds the floored budget into the
  `generationConfig` that `ModelShape::to_params` already produced, beside `thinkingConfig`
  — and the maxed-knobs shaping (`batch_shaping`) was reused verbatim across both.
- **Results come back inline in the long-running-operation object**, not behind a separate
  results URL (Anthropic's `results_url` + JSONL). No second fetch.
- **`batchStats` counts arrive as JSON strings** (`"7"`), not numbers — `as_u64` alone
  would have silently read them as 0, exactly the quiet miscount the project forbids. The
  count reader handles both.
- **Cancel is instant-terminal**, not Anthropic's interim "canceling" you poll through. A
  cancelled/failed/expired Gemini batch is `done:true` immediately with a top-level
  operation error and (usually) no per-item output. `Done(vec![])` would render as
  "0 results" and read like success — wrong — so added a `BatchPoll::Failed { state,
  message }` variant that names the terminal state honestly. Partial results that *did*
  land before the terminal event are still handed back as `Done`.
- **List lives under `operations`** (not `data`) and paginates via `nextPageToken` (not
  `has_more`); an empty list omits the key entirely, so that's an empty page, not an error.
- **The slashed provider id just worked.** A Gemini handle is `gemini/batches/<id>` — the
  id itself contains a slash — but `parse_batch_handle`'s split-on-*first*-slash plus the
  no-slash-in-backend-name invariant (enforced at config load) already made that
  unambiguous. The Anthropic-era design absorbed it for free.

`includeThoughts` is on (inherited from the shaping), so a Gemini answer carries the
reasoning as separate `"thought": true` parts; confirmed against a live thinking batch
that the answer text is the *non*-thought parts (a final answer part may still carry a
`thoughtSignature` — that's not a thought part, so it stays). The whole seam is
offline-tested (pure shaping/parsing fns + ScriptedBatch), and the live wire was proven
end-to-end through the actual MCP server: submit → list → poll → `## [0] Pong.`, plus a
cancel that lands in `Failed`, plus a non-batch cast refused with the supported set named.

**The dogfood earned its keep twice.** Running a real `gemini-batch` job through the
reconnected server surfaced that the cast's pinned synth `gemini-3-pro-preview` was
*retired* — and exposed a sharp edge of Gemini's batch lane: submit **accepted** the dead
model (`BATCH_STATE_PENDING`), and the failure only landed as a *per-item* error when the
request actually ran ("This model … is no longer available"). The per-item failure
surfacing did exactly its job — the answer came back as `## [0] — failed: provider error:
…` rather than a silent empty — but it's a reminder that Gemini batch does no model
validation at submit time. The fix is the drift-resistant default: the cast synths
`gemini-pro-latest` (the alias, today resolving to `gemini-3.1-pro-preview`) rather than a
pinned preview that gets retired out from under us — `provider-model-ids` drift, made
concrete. Confirmed live: `gemini-pro-latest`, `gemini-3.1-pro-preview`, and `gemini-2.5-pro`
all 200 on a synchronous `generateContent`; `gemini-3-pro-preview` 404s.

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
