# kaibo agent models — a quick comparison test

A small, single-question probe of how kaibo's four providers answer the *same*
`consult` question about this very repo. **This is not a benchmark** — it is one
question, one run per provider (n=1), with kaibo's default prompts and model
pairings. Treat it as a smoke test and a feel for each provider's depth, not a
ranking you can lean on. Quotes below are **verbatim** tool output.

- **Date:** 2026-06-06
- **kaibo commit:** `ce40a79` (thinking-on re-run; supersedes a first pass taken at
  `76ec2a9` *before* thinking was enabled — see "What changed with thinking on").
- **Thinking:** ON, both phases (the current default). Anthropic `thinking`
  (budget 8192), Gemini `thinkingConfig` (budget 8192), DeepSeek/Gemma reason
  natively. `max_tokens` 16384.
- **Harness:** kaibo as a stdio binary (`examples/ask_once`, removed after) calling
  `consult` directly so the freshly-built thinking code was exercised; the
  in-session MCP server predated thinking. `--root` = this repo.

## Methodology

**The single question, identical for every provider:**

> What is the DENYLIST in src/sandbox.rs and which builtins does it block? Name
> the file:line and explain why each is on the list.

Chosen because it has a crisp, checkable factual core (a constant and its line)
*and* a deeper "why" that rewards actually reading the surrounding doc-comment —
so it separates "grep the constant" from "understand the design".

**Ground truth** (from `src/sandbox.rs`, read directly, not via a model):

- `src/sandbox.rs:45` — `pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];`
- The doc-comment at `src/sandbox.rs:38–44` (and the module header) gives the real "why":
  - **`touch` and `mktemp` are the only ones actually compiled in** under the
    `localfs`-only build; they bypass the read-only mount via `std::fs`
    (mtime / real temp files), so the denylist is their *live* runtime defense.
  - **`git` / `spawn` / `exec` / `kill` are defense-in-depth.** Their feature
    axes (`git`, `subprocess`) are off, so they're never compiled in —
    `registry.get(name)` returns `None` and they're skipped. (`kill` is on the
    `subprocess` axis, not `host`.) They stay listed so the block is already in
    place if someone enables those axes later.
- Mechanism (`src/sandbox.rs:47–80`): each denied builtin is shadow-replaced with a
  `Blocked` wrapper that keeps the real name/schema but `execute`s to
  `ExecResult::failure(126, "{name}: disabled in kaibo's read-only sandbox")`.

**Grading rubric** — five things a complete answer gets right:

1. **List + line** — all six builtins, cited at `:45`.
2. **Live vs. defense-in-depth** — distinguishes `touch`/`mktemp` (compiled in,
   live) from `git`/`spawn`/`exec`/`kill` (off-axis, not compiled, DiD).
3. **Mechanism** — the `Blocked` shadow-wrapper, exit code 126, the message.
4. **Per-builtin rationale accuracy** — *why* each bypasses the mount, correctly.
5. **No errors** — nothing confidently wrong.

**Provider model pairings** (kaibo's `default_models`, explorer → synth):

| Provider | Explorer (cheap) | Synth (strong) |
|---|---|---|
| `anthropic` | `claude-haiku-4-5` | `claude-sonnet-4-6` |
| `deepseek` | `deepseek-v4-flash` | `deepseek-v4-pro` |
| `gemini` | `gemini-flash-lite-latest` | `gemini-3.5-flash` |
| `openai` (local default) | `Gemma-4-E4B-it-GGUF` | `Gemma-4-26B-A4B-it-GGUF` |

Defaults: explorer `max_turns=50`, synth `max_turns=8`. The three hosted
providers ran concurrently; the local `openai` endpoint (Lemonade/Gemma) ran
locally.

## Results (verbatim, thinking on)

### Anthropic — `claude-haiku-4-5` → `claude-sonnet-4-6`

> ## The `DENYLIST` in `src/sandbox.rs`
>
> ### Definition
>
> **`src/sandbox.rs:45`**
> ```rust
> pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];
> ```
>
> This is a compile-time constant slice of six builtin names. The doc-comment immediately above it (`src/sandbox.rs:36–41`) explains the design intent:
>
> > *Builtins that bypass the read-only backend to touch real state directly, so the `LocalFs::read_only` mount can't stop them (audited for `std::fs`/`git2`/`Command`/signal use).*
>
> ---
>
> ### Why Each Builtin Is Blocked
>
> | Builtin | Underlying mechanism bypassed | Status in `localfs`-only build |
> |---|---|---|
> | **`git`** | Uses the `git2` crate to read/write `.git` repository state directly | Compiled **out** (git axis is off) — on list for defense-in-depth |
> | **`touch`** | Modifies real file timestamps via `std::fs` mtime, bypassing the `LocalFs` read-only mount | **Actually compiled in** — actively blocked at runtime |
> | **`spawn`** | Launches subprocesses via `std::process::Command` | Compiled **out** (subprocess axis is off) — on list for defense-in-depth |
> | **`exec`** | Direct process execution via `std::process::Command` | Compiled **out** (subprocess axis is off) — on list for defense-in-depth |
> | **`kill`** | Signal/process management (OS-level process signals) | Compiled **out** (subprocess axis is off) — on list for defense-in-depth |
> | **`mktemp`** | Creates real temporary files on the host filesystem via `std::fs`, bypassing the read-only mount | **Actually compiled in** — actively blocked at runtime |
>
> ---
>
> ### How the Block Is Enforced
>
> **`src/sandbox.rs:73–79`** — the `apply_denylist` function wraps each denied builtin in a `Blocked` struct. This wrapper:
> - **Preserves the tool's schema** (so argument validation still works correctly),
> - **Returns exit code `126`** with the error message: `"{name}: disabled in kaibo's read-only sandbox"`.
>
> Exit code 126 is the conventional Unix "command found but not executable" code, which is a deliberate, recognisable signal rather than a generic failure.
>
> ---
>
> ### Summary of the Design Philosophy
>
> The list is divided into two practical categories:
>
> 1. **`touch` and `mktemp`** — these are *genuinely active threats* in the `localfs`-only build because they write through `std::fs` directly to the real host filesystem, circumventing the `LocalFs::read_only` mount entirely.
>
> 2. **`git`, `spawn`, `exec`, `kill`** — these live on feature axes that are compiled **off** by default, so `register.get()` already returns `None` for them and they'd be skipped anyway. They remain in `DENYLIST` purely as **defense-in-depth**: if those axes are ever re-enabled, the sandbox immediately blocks them without any additional code changes.

### DeepSeek — `deepseek-v4-flash` → `deepseek-v4-pro`

> ## `DENYLIST` in `src/sandbox.rs`
>
> ### Definition
>
> **`src/sandbox.rs:45`** declares the constant:
>
> ```rust
> pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];
> ```
>
> It is applied at **`src/sandbox.rs:73–78`** via `apply_denylist`, which iterates over each name and, if the builtin exists in the registry, replaces it with a `Blocked` wrapper (defined at **`src/sandbox.rs:49–66`**) that returns exit code 126 with the message `"<name>: disabled in kaibo's read-only sandbox"`.
>
> ### The six blocked builtins and why
>
> | Builtin | Primary reason | Category |
> |---------|---------------|----------|
> | **`touch`** | Calls `std::fs` to modify file mtimes **directly**, completely bypassing the read-only VFS backend (`LocalFs`). The VFS cannot intercept it. Called out at **`src/sandbox.rs:17–18`** as a "live" threat under the `localfs`-only build. | **Active threat** |
> | **`mktemp`** | Creates real temporary files on disk via `std::fs`, again **bypassing the VFS layer**. Same class as `touch`; explicitly named alongside it at **`src/sandbox.rs:17–18`**. | **Active threat** |
> | **`git`** | Would reach real repository state through the `git2` library. Not compiled in under the current `localfs`-only feature set (`Cargo.toml:24`), but kept in the denylist as **defense-in-depth** in case someone enables the `git` feature axis later (**`src/sandbox.rs:36–42`**). | Defense-in-depth |
> | **`spawn`** | Spawns arbitrary subprocesses via `std::process::Command`. Not compiled (requires the `subprocess` feature axis); defense-in-depth. | Defense-in-depth |
> | **`exec`** | Executes external commands. Not compiled (requires `subprocess`); defense-in-depth. | Defense-in-depth |
> | **`kill`** | Sends signals to processes. Not compiled (requires the `host` feature axis); defense-in-depth. | Defense-in-depth |
>
> ### The layered rationale
>
> The module-level doc at **`src/sandbox.rs:4–20`** describes three overlapping safety layers:
>
> 1. **Compile-time** (primary): Only the `localfs` feature is enabled — `spawn`, `exec`, `kill`, and `git` are never even compiled.
> 2. **VFS layer**: The project root is mounted as `LocalFs::read_only`, which blocks filesystem-mutating builtins that *do* go through the VFS backend (`rm`, `mv`, `cp`, `mkdir`, `tee`, `write`).
> 3. **`DENYLIST`**: Catches the two builtins — `touch` and `mktemp` — that **bypass the VFS entirely** by calling `std::fs` directly, plus provides defense-in-depth for the four feature-gated builtins.

### Gemini — `gemini-flash-lite-latest` → `gemini-3.5-flash`

> In `src/sandbox.rs:45`, the `DENYLIST` is defined as:
>
> ```rust
> pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];
> ```
>
> ### Context / Why the DENYLIST Exists (`src/sandbox.rs:38-44`)
> The doc comment immediately preceding the definition (`src/sandbox.rs:38-44`) explains that these builtins bypass the read-only backend to interact with or modify the system's real state directly. Because the `LocalFs::read_only` mount cannot stop them (since they are implemented using direct APIs like `std::fs`, `git2`, `Command`, or process signals), they are shadow-blocked as a **defense-in-depth** measure.
>
> Under the default `localfs`-only build, only `touch` and `mktemp` are compiled. The other tools (`git`, `spawn`, `exec`, and `kill`) belong to "heavier axes" (features) that are currently disabled. However, they are maintained on the `DENYLIST` so that they will be automatically blocked if those features are enabled in the future.
>
> ---
>
> ### Why Each Builtin is on the List
>
> 1. **`git`**
>    * **Reason:** It directly interacts with the version control system and filesystem state, which violates sandbox guarantees and read-only constraints.
> 2. **`touch`**
>    * **Reason:** Modifies file metadata (such as access or modification times) directly on the filesystem, bypassing read-only protections.
> 3. **`spawn`**
>    * **Reason:** Spawns and runs external subprocesses directly, which poses a severe security/isolation hazard.
> 4. **`exec`**
>    * **Reason:** Replaces the current process image or executes an external process directly, compromising sandbox boundaries.
> 5. **`kill`**
>    * **Reason:** Sends signals directly to external OS processes, which is disallowed to prevent process interference/manipulation.
> 6. **`mktemp`**
>    * **Reason:** Generates and writes temporary files or directories directly, violating the read-only file system restriction.

### `openai` (local) / Gemma-4 via Lemonade — `Gemma-4-E4B-it-GGUF` → `Gemma-4-26B-A4B-it-GGUF`

Gemma already reasoned by default (lemonade's `--reasoning-format auto`), so
thinking-on is no change for it — quoted here for completeness. (Latency note: this
run finished much faster than the first Lemonade test, ~74 s vs ~324 s, after the
local server's context window was enlarged.)

> The `DENYLIST` in `src/sandbox.rs` is defined on line 45:
>
> `src/sandbox.rs:45: pub const DENYLIST: &[&str] = &["git", "touch", "spawn", "exec", "kill", "mktemp"];`
>
> It blocks the following builtins:
> * **`git`**
> * **`touch`**
> * **`spawn`**
> * **`exec`**
> * **`kill`**
> * **`mktemp`**
>
> **Rationale:**
> According to the documentation in `src/sandbox.rs`, these builtins are included in the `DENYLIST` to "shadow-block the builtins that reach real state *directly*" (as noted in the comment near line 44). Each of these commands is restricted because they would otherwise interact with or modify the underlying system state directly rather than staying within the sandbox.

## Analysis

Scorecard against the five-point rubric (✓ = met, ~ = partial, ✗ = missed/wrong):

| | List+line | Live vs DiD | Mechanism | Rationale accuracy | Errors |
|---|:---:|:---:|:---:|:---:|:---:|
| **Anthropic** | ✓ | ✓ | ✓ | ✓ | none |
| **DeepSeek** | ✓ | ✓ | ✓ | ✓ | 1 minor |
| **Gemini** | ✓ | ✓ | ✗ | ~ | none |
| **Gemma-4 (local)** | ✓ | ✗ | ✗ | ~ | none |

- **Anthropic and DeepSeek remain the top tier.** Both reconstruct the actual
  design — `touch`/`mktemp` live vs `git`/`spawn`/`exec`/`kill` defense-in-depth —
  and both describe the `Blocked` wrapper with exit 126 and the message. Anthropic
  adds the nice "126 = command found but not executable" gloss. DeepSeek's one slip:
  it pins `kill` to the `host` axis; it's actually `subprocess` (everything else is
  right, including `Cargo.toml:24`).
- **Gemini** still doesn't describe the `Blocked`/exit-126 mechanism and its
  per-builtin "why" list stays generic — but it now correctly states that only
  `touch`/`mktemp` are compiled and the rest are future-proofing, i.e. it caught the
  live-vs-DiD distinction. No false claims this time.
- **Gemma-4 (local)** is unchanged: headline-correct (list + `:45`), error-free, but
  thin — no live-vs-DiD, no mechanism, generic rationale.

### What changed with thinking on

Versus the earlier pre-thinking pass (commit `76ec2a9`, same question):

- **Gemini improved the most.** Pre-thinking it gave the only outright-wrong
  rationale — claiming the read-only policy "otherwise enforces" the `touch` block
  (backwards; `touch` is listed *because* it bypasses the mount). With thinking on,
  that error is gone **and** it newly surfaced the live-vs-defense-in-depth split it
  had missed. The clearest win for thinking here.
- **Anthropic and DeepSeek** were already at the ceiling on this question; thinking
  kept them there (DeepSeek picked up a trivial `kill`-axis slip — within run-to-run
  noise at n=1, not a thinking regression).
- **Gemma** was already reasoning in both passes, so no change — still correct but
  shallow.

**Latency / cost (rough, wall-clock — not separately instrumented):** the three
hosted providers returned together in ~55 s (an earlier separate run). Gemma took
~74 s but is **free and fully local** — nothing leaves the box, no API key.

## Caveats & threats to validity

- **n = 1.** One question, one run per provider. No retries, no variance estimate;
  the DeepSeek `kill`-axis slip is exactly the kind of single-run noise that warns
  against over-reading the scorecard.
- **One question shape.** This probe rewards reading a doc-comment. Other tasks
  (multi-file tracing, "where is X used", refactor reasoning) could reorder the field.
- **Default prompts and model ids.** Pairings are kaibo's `default_models`, which
  drift (see `docs/issues.md`). No per-provider prompt tuning.
- **Grading is one reader's judgment** against the source, not scored blind. The
  rationale-accuracy column is the most subjective.
- **Thinking budgets are static** (8192 / `max_tokens` 16384), not tuned per model.

## Takeaway

For this "explain the design, with citations" question: **Anthropic and DeepSeek
give the deepest, most accurate answers**, reach for them when the *why* matters.
**Thinking on visibly helped the weaker model** — it fixed Gemini's one factual
error and pulled it up to the live-vs-defense-in-depth distinction, which is the
single best argument for keeping it on by default. **Gemma-4 (local) stays a solid,
private, free default for the factual core** but tops out at headline depth on this
kind of question. None of this is conclusive from one question — re-run with a
broader question set before trusting the ordering.
