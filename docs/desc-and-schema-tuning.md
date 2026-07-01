# Description & schema tuning — making kaibo legible to the agents that call it

*Working doc, started 2026-07-01. This is the plan for a coordinated pass over every
piece of model-facing text kaibo ships — the handshake instructions, tool names,
descriptions, and input schemas — plus one new capability the moment calls for. Delete
sections as they ship (issues.md discipline); melt durable rationale into AGENTS.md /
docs/casts.md when done.*

## Why now

Fable-class models are here and expensive to run interactively — subscription access
comes in short windows, and the affordable lane is the **batch API** (half price,
async). kaibo already owns the hard parts of "have a frontier model deliberate over
your codebase without babysitting a session": a read-only investigation loop, durable
batch handles that survive a restart, and knob-maxing on the batch lane
(`batch.rs::batch_shaping` — effort forced high, completion budget floored). What's
missing is the one tool shape that connects them, and a tool surface legible enough
that *other people's agents* can find it cold. That legibility is part of the
release-ease gate: our users are agents, and an agent's install experience *is* the
handshake and the schemas.

The trigger for the text pass: we sat in the caller's seat (Claude Code, 2026-07-01)
and looked at what actually reaches the calling model. It is not what we designed for.

## What a host actually shows the calling model (observed)

Three tiers, with different cost and reach:

1. **Resident every session** (billed whether kaibo is used or not):
   - The `instructions` from the initialize handshake — **truncated by Claude Code**
     at exactly **2048 characters per server** (hardcoded, not configurable; confirmed
     by extracting the minified source from the installed binary, v2.1.198), cut with
     a literal `… [truncated]` marker. What
     survived: the lead paragraph, the full `## Casts` roster, the first lines of the
     kaish onboarding. What was lost: the rest of the kaish reference, the
     `## Learn more kaish` resource pointers, and the entire **`## Scope` section** —
     so the containment posture the handshake exists to surface
     (`kaish_syntax.rs::kaibo_instructions_with_scope`) never reached the model. The
     "menu before reference" ordering anticipated truncation and saved the casts; it
     did not save scope, which sits below the kaish wall.
   - Tool **names only**. Claude Code now defers MCP tool schemas: the model sees
     `mcp__kaibo__consult`, `…get`, `…wait` as bare names and must spend a lookup to
     read a description before calling. Names are load-bearing in this regime.
2. **On demand**: tool descriptions + schemas once fetched; resources
   (`kaibo://config`, `kaibo://tools`, `kaibo://kaish/*`) only when the agent asks —
   hosts never push them. A resource nobody's surviving prose names does not exist.
3. **Call time**: results, the provenance footer, `structured_content`. (And per the
   notification-channels finding: logging/progress never reach the calling agent in
   Claude Code — `wait` is the channel, which it already is by design.)

Other hosts differ — some inline every description all the time (there the "density
is existential" doctrine applies with full force), some may ignore `instructions`
entirely (there the descriptions are the *only* prose a model ever sees). Two Sonnet
research passes are out to pin the exact numbers: Claude Code's truncation cap and
deferral threshold, and a per-host survey (Claude Desktop, Codex CLI, Gemini CLI,
Cursor, VS Code). **Fill in here when they report:**

- **Claude Code instructions cap: 2048 characters** (UTF-16 code units, JS `.length`),
  applied **per server** right after `getInstructions()` — no shared budget across
  servers, no env/settings override, no warning to the user (a debug log line only).
  Confirmed against the installed binary (v2.1.198, constant `hQ=2048`). Upstream
  issue anthropics/claude-code#43474 reports the symptom but theorizes a shared
  cross-server budget — the code says flat per-server 2048; worth a correcting
  comment there when we engage.
- **Each MCP tool `description` gets its own independent 2048-char cap**, same
  constant and marker. Our longest description (`consult`, ~1 KB) is comfortably
  under; keep it that way.
- **Tool-schema deferral (ToolSearch) is ON by default** for all MCP tools on
  Sonnet-4+/Opus-4+/Haiku-4.5+ — controlled solely by the `ENABLE_TOOL_SEARCH` env
  var (unset ⇒ deferred; `false` restores load-everything; `auto[:N]` uses a
  ~10%-of-context threshold). A server can opt a tool out via
  `_meta["anthropic/alwaysLoad"]` — a knob worth considering for `consult` so the
  front-door description is resident even in the deferred regime.
- **Per-host survey** (2026-07-01, sourced from each host's code where open):

  | host | `instructions` → model? | tool descriptions |
  |---|---|---|
  | Claude Code | yes, truncated at 2 KB; **also the tool-search retrieval key** | deferred by default, 2 KB cap each |
  | Claude Desktop | **never** — parsed, zero call sites read it | resident; the only prose the model sees |
  | Codex CLI | yes, **verbatim** — becomes the tool-namespace description *and* search-index text | deferred when its tool search is on |
  | Gemini CLI | yes, verbatim, into the first user message (`<project_context>`, trust-gated) | always resident |
  | VS Code Copilot | yes, verbatim, into the system prompt per server | resident (grouping only kicks in above 64 tools); **1024-char cap on schema *property* descriptions under GPT-4-family models** |
  | Cursor / Zed / Windsurf | undocumented — assume ignored | Cursor defers (names only); others resident |

  Load-bearing conclusions: **(1)** Claude Code's 2048 is the *only* ceiling anywhere
  — every other host that uses the field takes it verbatim, so writing to 2 KB costs
  nothing elsewhere. **(2)** Claude Desktop (and assume Cursor/Zed/Windsurf) never
  shows instructions to the model — each tool description must stand entirely alone.
  **(3)** In both deferral hosts (Claude Code, Codex) the instructions double as the
  **search index** that decides whether the model ever fetches our tools — the
  opening lines aren't just prose, they're retrieval keys; front-load the words an
  agent would search for ("codebase", "review", "second opinion", "read-only",
  "batch"). **(4)** Keep every schema *property* description under ~1 KB (VS Code /
  GPT-4 cap) — ours already comply. Tool-count caps (Cursor ~40) are moot for our
  nine — a real advantage worth keeping.

## Budgets & principles for the pass

- **2048 characters of instructions survive in Claude Code — treat it as the hard
  budget.** Everything a caller needs to *decide* (what kaibo is, which team, what's
  allowed, that an async lane exists) goes above that line; everything a caller needs
  to *execute* (kaish idioms, override semantics) lives in schemas and resources,
  named from above the line. This is *tight*: today's lead (~950 chars) + casts
  roster (~800 with a 10-cast config) already brush the ceiling before scope gets a
  byte — the lead and roster framing need compression too, not just reordering. Note
  the roster scales with the operator's config; the budget test should use a
  representative multi-cast config, not the minimal one.
- **Names must self-describe.** In the deferred regime a name is the whole pitch.
- **Instructions are retrieval keys, not just prose.** Claude Code and Codex feed
  them into the index their tool search matches against; the handshake's opening
  lines determine whether a deferred kaibo gets *found* at all.
- **Claude Desktop ignores `instructions` — confirmed, not hypothetical.** Each tool
  description must stand alone: a model that has read nothing else should still pick
  the right tool.
- **One authoritative home per fact.** The cast roster currently ships ~3×
  (instructions, four description tails, the `cast` enums). Redundancy that survived
  truncation by luck is not a strategy.
- Existing doctrine holds: client-facing text terse, agent-facing text verbose where
  it shapes behavior, re-read every block holistically (AGENTS.md, "Writing for
  models").

## The work

### 1. Restructure the handshake (highest value per byte)

New order, budget-tested: *(optional setup banner)* → lead → `## Casts` →
**`## Scope`** → one-sentence async-lane pointer → compressed kaish gist.

- **Scope moves above the kaish wall.** Containment posture is trust surface — it's
  the second thing a cautious caller (or its human) wants to know.
- **The kaish onboarding shrinks to a gist** — a paragraph plus pointers to
  `kaibo://kaish/syntax` / `builtins` / `sandbox`. Rationale: the primary caller path
  (`consult`) never writes kaish; only a `run_kaish` driver needs syntax, and that
  driver can afford one resource read. The full `agent_onboarding` spine stays
  available as a resource; it stops being resident.
- **TDD:** extend the `instructions_*` test family with a budget test — the text
  through the end of `## Scope` fits under **2048 chars** (count as chars, the way
  Claude Code does, against a representative multi-cast config) — and an ordering
  test (scope precedes the kaish gist). Failing first against today's layout.

### 2. Rewrite the lead

Today it names `consult` / `oneshot` / `run_kaish`; six of nine tools (the whole
async lane) are invisible in resident prose, and the roster's `(batch)` tags dangle
with nothing connecting them to `batch_submit`. Add one earning sentence, e.g.: *"For
work you don't wait on: `consult_submit` and `batch_submit` return handles;
`wait`/`get`/`list`/`cancel` manage them."* — and make the `(batch)` tag in the casts
section point at the batch lane by name.

### 3. Rename the generic tools

**Decided (Amy, 2026-07-01): the `job_` prefix scheme.** Uniform, self-namespacing
even in hosts that flatten tool names, and the verbs stay the familiar ones:

| today | becomes |
|---|---|
| `get` | `job_get` |
| `list` | `job_list` |
| `wait` | `job_wait` |
| `cancel` | `job_cancel` |
| `consult_submit`, `batch_submit` | keep — they're variants of consult/batch that *produce* jobs; the `job_*` quartet manages them |

**Migration:** MCP has no tool-alias mechanism. A rename lands in one release with a
loud CHANGELOG entry and a bumped minor version; the old name simply vanishes from
`tools/list`, which every host re-reads on connect, so the blast radius is retrained
habits and any user allowlists (`mcp__kaibo__get` → new name). Worth doing before the
audience widens, not after — that's the whole argument for doing it *now*.

### 4. `deliberate` + `explore` — the dossier machinery, worn two ways (the big one)

**Shape:** an interactive, cheap-but-capable model (Sonnet-class) runs the
consult-style investigation loop against the read-only project and produces a
**dossier** — the question sharpened, the load-bearing spans quoted with `file:line`,
whole files inlined where the question reaches across one (the `attach` machinery
already inlines files for tool-less prompts). kaibo then submits *dossier + question*
to a heavyweight synth for offline deliberation — knobs maxed, no session to babysit
— and hands back a handle. Collect with `job_wait`/`job_get`.

**`explore` returns (Amy, 2026-07-01):** the dossier-builder, exposed directly. The
old standalone explore folded into consult as `explore′`; it comes back as a
first-class tool that returns the **structured, cited report itself** rather than an
answer — for mapping unfamiliar code, or for a caller that wants to inspect/refine
the dossier before sending it onward. `deliberate` is then literally
`explore → offline synth`; one machinery, two tools, and the composition is visible
to the caller. Rounds out the ladder: `run_kaish` (no model) → `explore` (cheap
model, report) → `consult` (capable model, answer) → `deliberate` (heavyweight
model, offline).

**Two deliberation lanes, one tool (Amy, 2026-07-01):** the synth lane is a
*strategy*, not a provider feature.

- **Provider batch** — frontier model (Fable), max thinking, half price, durable
  `backend/provider-id` handle that survives restarts.
- **Local direct** — a large-parameter model on big unified memory (the Strix Halo
  class: GLM-4.5-Air, Qwen3-Coder-Next already in the casts) runs one long
  completion and *takes the time it takes*. Free, private, hours are fine. Handle is
  a session-scoped `job-N` (restart survival stays out of scope per the standing
  daemon decision — an hours-long local job lost to a restart is real pain, so say
  the session-scoping *loudly* in the schema, and revisit only with the daemon
  question). Reaffirmed 2026-07-01 (Amy): kaibo restarts have been dev-driven, not
  crashes — defer persistence until the use case hurts; a sqlite job store is the
  someday-shape, not today's.

The dossier split earns its keep *twice* on local: a big local model is slow per
token, so burning turns on tool-loop round-trips is the worst place to spend it —
and local context windows are small, so a curated dossier fits where an interactive
transcript wouldn't (cf. the gemma-ctx finding). Same pitch both lanes: frontier-
quality deliberation without interactive prices. Practicalities for the local lane:
per-backend `request_timeout` must stretch to hours, no retry (policy already), and
one deliberation occupies the llama.cpp server — the provider queues, kaibo doesn't.

Why this is kaibo-shaped: it's `consult`'s explorer→synth split stretched across the
sync/async boundary. The batch synth is tool-less *by construction* (batch requests
can't loop), so context acquisition must **complete** before submit — exactly the
discipline the dossier enforces. Everything reuses an existing seam:

- the investigation loop and report aggregation are `run_phase` + the `explore′`
  machinery in `consult.rs`;
- the wire path, knob-maxing, and per-item failure handling are `batch.rs`;
- the handle/collect lifecycle is `jobs.rs` + the unified `get`/`wait`/`cancel`.

**Design questions to settle (with Amy):**

- **Name.** `deliberate` — in working use, unchallenged; treat as settled unless a
  better one shows up before the tool lands.
- **Two-stage handle.** The interactive dossier phase takes real minutes before any
  batch exists — and the local lane never produces a provider handle at all. So:
  return a `job-N` immediately, always; on the batch lane it *becomes* a durable
  `backend/provider-id` at submit (`job_wait`/`job_get` narrate the transition and
  hand back the durable handle so the caller can hold it across restarts). The local
  lane stays `job-N` end-to-end.
- **Cast/config shape.** Today batch-ness is a property of a whole cast
  (`cast_is_batch`); `deliberate` needs an *interactive* explorer slot paired with an
  *offline* synth slot, and the synth's lane is now **batch | direct** (e.g.
  `[casts.fable]` explorer → `anthropic/claude-sonnet-4-6`, synth →
  `anthropic/claude-fable-5` on the batch lane; `[casts.halo]` explorer → a small
  local model, synth → a big one, direct). Likely move: lane becomes a per-slot
  property and today's batch casts are the degenerate case (no explorer). Needs its
  own holistic look at `config.rs`.
- **Fan-out.** One dossier can serve many questions (`prompts`-style) and many
  synths (a cross-model study at batch prices: same dossier to Fable, Gemini Pro,
  DeepSeek). Probably v2 — but don't design the args so it can't grow there.
- **Does it subsume `batch_submit`?** `deliberate` with a no-op investigation
  (caller-owned context) *is* `batch_submit`. Maybe eventually; keep both until the
  new shape proves out.

**Failure mode to design against:** a thin dossier sends Fable deliberating on air.
The context framing already installed for consult (trusted evidence, extend rather
than re-verify, name the edge of the evidence) applies, plus a dossier-side norm:
prefer whole files over snippets — batch tokens are cheap, and the synth can't go
back for more.

**TDD:** the scripted `CompletionClient` drives the dossier phase offline; the batch
body builders in `batch.rs` are pure, so the submitted wire shape (dossier placement,
maxed knobs) gets pinned without a network. Failing-first on the seam that doesn't
exist yet: dossier → batch item.

### 5. Schema & description hygiene

- **Strip maintainer-facing text from shipped schemas.** Every input struct's
  top-level `description` ships "See [`ConsultInput`] for the `deny_unknown_fields`
  rationale" — a rustdoc cross-reference no caller can resolve. Say the caller-useful
  part once where it lives ("a typo'd argument fails loud rather than running on
  defaults") and keep the rustdoc plumbing out of the wire schema (doc-comment
  restructure, or schemars-level scrub).
- **Drop the "Casts ready now: …" description tails.** The `cast` param enum carries
  the same names authoritatively and per-lane; the instructions roster carries the
  models. Four copies → two.
- Then the standing rule: re-read every touched description whole, judge
  holistically, compress.

### 6. Encode the budget in AGENTS.md

The "Writing for models" section's client-facing bullet predates the measurements;
update it so every future agent inherits the numbers. Draft replacement for the
client-facing bullet (keep the agent-facing one as is):

> - **Client-facing text** — the MCP server instructions and each tool's
>   `description` in `server.rs`, read by the *calling* agent. Hard numbers
>   (measured 2026-07-01): Claude Code truncates the instructions **and each tool
>   description at 2048 characters** (per server, hardcoded, silent). Claude
>   Desktop never shows instructions to the model at all, so **every description
>   stands alone**. In deferral hosts (Claude Code, Codex) the instructions are
>   also the tool-search **retrieval index** — the opening lines decide whether our
>   tools get *found*. So: the first 2048 characters are the whole resident pitch.
>   Decisions above the line (what kaibo is, the casts, scope, that an async lane
>   exists); execution detail lives in schemas and resources *named from above the
>   line*; front-load the words a working agent would search for. The
>   `instructions_*` budget tests in `kaish_syntax.rs` enforce the ceiling — a new
>   clause must displace an old one.

## The target (drafts to converge on)

Concrete text to build toward — supersedes the sketches in §1–§3 where they differ.
Character counts are measured; the handshake total is **~1,850 of 2048** (with a
10-cast roster and all eleven tools named in the lead — the budget test pins this
against a representative config). Drafts use the decided `job_*` renames and include
`explore` and `deliberate`; adjust here first if a decision changes.

**Handshake** (three blocks; the kaish onboarding leaves it *entirely* — `run_kaish`'s
own description plus the `kaibo://kaish/*` resources carry the shell. Note: content
*below* Scope is only lost on Claude Code — verbatim hosts show it, so a short kaish
gist could ride below the fold; it still bills per-session on those hosts, so add it
only if it earns rent):

> kaibo (解剖) — grounded, cited answers about a codebase from a model outside your
> own family. DeepSeek, Gemini, Anthropic, or a local model reads the project
> READ-ONLY and answers with file:line citations. Say in prose what you did or want
> to know — kaibo finds and reads the current code itself; no pasted files or diffs
> needed. `consult` is the front door. `explore` returns a cited survey report
> instead of an answer. `oneshot` is a toolless second opinion when you own the
> context. `run_kaish` drives the read-only shell directly. Work you don't wait on:
> `deliberate` (a frontier or large local model reasons offline over an investigated
> dossier), `consult_submit`, and `batch_submit` return handles;
> `job_wait`/`job_get`/`job_list`/`job_cancel` manage them. *(760 chars)*
>
> ## Casts *(roster as today, tighter framing — ~680 chars at 10 casts; keep the
> `(local, unverified)` honesty tags)*
> A cast is the model team that answers; pass `cast=<name>`. Usable now (resolved at
> startup — reconnect after config/key changes): *(…roster lines with synth models,
> as shipped…)*
>
> ## Scope *(404 chars)*
> Read-only, always: kaibo never writes and cannot run external commands. A call's
> `path` must fall under an allowed tree:
> - `/home/atobey/src/kaibo` (default root — calls may omit `path`)
>
> Go deeper without spending a turn: `kaibo://config` (full resolved config, casts,
> backends, limits), `kaibo://tools` (attachments, overrides, the async workflow),
> `kaibo://kaish/*` (shell syntax and idioms).

**Tool descriptions** — each stands alone (Desktop shows nothing else), opens with
its retrieval keys, and drops the "Casts ready now" tail (the `cast` enum carries the
names). `consult` gets the `_meta["anthropic/alwaysLoad"]` pin so the front door is
resident even under deferral.

> **`consult`** *(702)* — Ask a model outside your own family about a codebase —
> code review, debugging, architecture, "what does this change break" — and get a
> grounded answer with `file:line` citations. A capable model (DeepSeek, Gemini,
> Anthropic, or local — pick with `cast`) drives a READ-ONLY shell over the project:
> it reads the real, current source, delegates broad sweeps to a fast explorer, and
> answers with evidence, never modifying anything. Describe your intent in prose;
> kaibo locates the code itself, so you don't paste files or diffs. `attach` puts
> specific files in front of it; `session_id` threads a multi-turn consultation. For
> a toolless opinion use `oneshot`; to run in the background use `consult_submit`.

> **`oneshot`** *(447)* — Ask a model outside your own family a direct question —
> prompt in, answer out. No tools, no codebase access: the second-opinion primitive
> for when you already own the context. Paste what's needed, or `attach` whole files
> (kaibo inlines them, so their bytes never cross your context). Pick the answering
> team with `cast`. When kaibo should investigate the code itself, use `consult`; to
> fan many prompts offline at batch prices, use `batch_submit`.

> **`run_kaish`** *(492)* — Run a kaish (sh-like) script against the READ-ONLY
> project; returns exit code + stdout + stderr. Read generously with line numbers —
> `cat -n FILE` for a whole file, `grep -rn PATTERN .` to locate across files — and
> compose builtins with pipes (grep/jq/awk/find/...). Writes and external commands
> are refused (exit 126 = blocked, 124 = timed out); each call starts fresh at the
> project root. See `kaibo://kaish/*` (or `help` in the script) for idioms and the
> bash habits that don't carry over.

> **`explore`** *(472, new)* — Survey a codebase and get back a structured, cited
> report — not an answer. A fast, cheap model sweeps the project READ-ONLY (grep,
> whole-file reads) and returns a summary of findings, the relevant locations with
> `file:line`, and the trail it followed. The evidence-gathering half of `consult`,
> exposed directly: map unfamiliar code, or build the dossier you'll send onward
> (`deliberate` runs exactly this before its offline synth). For a synthesized
> answer, use `consult`.

> **`deliberate`** *(569, new)* — Put a top model's deepest reasoning on your
> codebase without holding a session open. A fast model first investigates the
> project READ-ONLY and assembles a cited dossier; a heavyweight synth then
> deliberates offline over that evidence — a frontier model on the provider's batch
> lane (max thinking, half price) or a large local model taking the time it takes.
> Returns a handle immediately: keep working, then `job_wait`/`job_get` it. Best for
> hard questions worth hours — a design review, a gnarly bug, "is this abstraction
> right". For an answer this turn, use `consult`.

> **`consult_submit`** *(397)* — Run a `consult` in the background: same read-only
> investigation, same arguments, but returns a `job-N` handle immediately. Fan out a
> cross-model study (one submit per cast, collect them all) or keep working while a
> deep consult runs. `job_wait` parks for results, `job_get` fetches them,
> `job_cancel` stops one. Handles live for this server session only. For an answer
> in this turn, use `consult`.

> **`batch_submit`** *(430)* — Fan self-contained prompts to a top-tier model on the
> provider's batch lane — offline, max thinking, half price. Like `oneshot`, no
> tools and no codebase access: each prompt carries its own context, or `attach`
> files shared by all. Returns a durable `backend/provider-id` handle that survives
> restarts: submit, go work, then `job_wait`/`job_get`. Needs a batch-capable
> cast/backend (you get a clear refusal naming them otherwise).

> **`job_get`** *(297, was `get`)* — Collect async work by handle — a batch
> (`backend/provider-id`) or a background job (`job-N`). Returns a progress line
> while it runs, the full result once done (batches: every item's answer, per-item
> failures surfaced). Collect occasionally rather than in a tight loop — nothing is
> lost by waiting.

> **`job_list`** *(316, was `list`)* — List your async work: background jobs in
> flight this session, and the batches the providers still know about (last 24h by
> default; `all: true` for everything) — each with a ready handle for
> `job_get`/`job_cancel`. This is the way back to a batch whose handle you lost: the
> provider's own list is the source of truth.

> **`job_wait`** *(311, was `wait`)* — Park for async work to make progress: blocks
> up to `timeout_secs` and returns as soon as a job finishes or fails, or on a clean
> timeout — the productive alternative to polling `job_get`. `level:"info"` adds the
> live narrative (each shell command, sweep, milestone); name batch `handles` to
> fold their status in.

> **`job_cancel`** *(226, was `cancel`)* — Stop a running async job by handle — a
> batch stops scheduling new items (in-flight ones finish); a background job aborts
> its investigation. `job_get` it afterward for the final state. A job that already
> finished is left alone.

What changed, surface-wide: every description now opens with retrieval keys instead
of mechanism; the async family is one legible verb set (`*_submit`/`deliberate`
produce jobs, the `job_*` quartet manages them); the `kaibo://tools`
cross-references drop from descriptions that don't need them; and no description
leans on the handshake existing. `explore` and `deliberate` are written before they
exist — the pitch is part of the design. The full ladder the surface teaches:
`run_kaish` (no model) → `explore` (cheap model, report) → `consult` (capable model,
answer) → `deliberate` (heavyweight model, offline).

## Sequencing

1. ~~Research~~ — landed 2026-07-01; numbers are in.
2. **One holistic surface PR** (Amy's call, 2026-07-01): handshake restructure +
   lead rewrite + renames + description rewrites to the target above + schema
   hygiene + the AGENTS.md budget guidance (§1, §2, §3, §5, §6 together). The
   surface is one composition read by one audience — reviewing it whole is the
   point. Budget/ordering tests failing-first; CHANGELOG-loud on the renames.
   The `explore`/`deliberate` descriptions (and their mentions in the lead) ship
   only with their tools — no phantom tools in `tools/list`.
3. §4 (`explore` + `deliberate`) — its own arc: config/cast reshape (per-slot lane:
   batch | direct), the dossier machinery landing once wearing both tools, each with
   the description already drafted above. `explore` can land first — its machinery
   (`report_preamble`, the explore′ loop) already runs inside consult.

Cross-family review on each per AGENTS.md; the `deliberate` arc gets the hard look.
