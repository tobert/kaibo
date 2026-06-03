# AGENTS.md — kaibo (解剖)

Critical orientation for agents working on kaibo. Short by design; the code and
`docs/issues.md` are ground truth.

## What kaibo is

A stdio MCP server with one tool, `consult`: ask a question about a codebase, get
a grounded, cited answer. Two-phase (the dpal pattern): a cheap **explorer** model
drives a **read-only** kaish shell via `run_kaish(script)` and writes a curated
report; a stronger **synthesizer** model answers from that report (with `run_kaish`
as a fallback). Multi-provider (Anthropic / DeepSeek / Gemini) over `rig-core`.
kaibo never modifies the project and cannot run external commands.

## Invariants — do not weaken without a failing-first test

- **Read-only is the product.** Enforced in `src/sandbox.rs` by four levers: a
  read-only mount, `MemoryFs` at `/`, external commands disabled, and a `DENYLIST`
  of builtins that reach real state *directly* and bypass the mount (git, touch,
  spawn, exec, kill, mktemp — see the module doc-comment). Any change here keeps
  `tests/sandbox.rs` green and adds a test that can fail.
- **stdio only.** kaibo can read a filesystem, so it must never bind a socket.
- **kaish is `!Send`.** The kernel runs on a dedicated thread behind `KaishWorker`;
  rig tools require `Send` futures. Don't hold the kernel across an `.await`.

## Working here

- **TDD.** Tests that can and will fail. The sandbox boundary gets failing-first
  tests — and we prove they have teeth (empty the `DENYLIST`, watch them fail).
- **`docs/issues.md` is the live tracker.** Skim it before new work. Delete
  entries when they ship — don't mark them done; git history is the record.
- **`kaish-kernel` is a path dep** (`../kaish/crates/kaish-kernel`), under active
  development. It will break kaibo's build transiently — adapt to its new API,
  don't pin around it. `kaish-mcp` is a useful reference sibling, not a dependency.
- **Provider model ids drift.** Defaults live in `consult.rs::default_models`;
  rig's bundled model consts are often retired. Cross-check the pal configs.

## Commit style

Commits explain **why, not what** — the diff already shows what changed. Write the
body as a short summary of the *decisions* behind the change and their rationale,
drawn from the working conversation: what we chose, what we rejected, and why (when
a why was stated). A few sentences of reasoning beat a bullet list of files.

- **Subject:** imperative, the decision or outcome — not "update sandbox.rs".
- **Body:** the reasoning and tradeoffs. Cite a decision's source when it matters.
- Don't narrate the code; point to `docs/issues.md` for follow-ups.
- End every message with:
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`

Example:

```
Sandbox the explorer behind a read-only kaish

The explorer must read the project but never mutate it. Chose kaish's read-only
VFS mount plus a denylist over a hand-rolled file API: the mount makes "read-only"
structural rather than honor-system. We confirmed git/touch bypass the mount
(unblocked, `git init` returned 0 and made a real .git), so those are shadow-
blocked at the registry too. Tests prove the boundary and that the blocks have
teeth.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
```
