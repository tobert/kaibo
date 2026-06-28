<p align="center">
  <!-- Kaile, the kaibo mascot. Brand assets + generator live in docs/brand/. -->
  <img src="docs/brand/banner-teal.png" alt="kaibo（解剖）" width="820">
</p>

<p align="center">
  <strong>A tool for getting second opinions.</strong>
</p>

<p align="center">
  <!-- TODO(repo): badges once published — crates.io version, license, CI. -->
</p>

---

## Introduction

Sometimes editing and code review are valuable because other people see things right away that
have slipped by us. Maybe it's because we were too close to the writing, or didn't sleep well
last night, or any number of reasons. This happens with agents too. A new model revision rolls
out, your model writes some bad code. It can't see the bug right in front of it.
The solution for both humans and agents is the same: get a second opinion.

kaibo is a suite of agents wrapped in an MCP server, and the agents run on a
different model family than yours. Biases tend to be shared across
a model lineage: the Claudes share them, the GPTs share them, and spawning more
subagents from the same family repeats them. It's a bit like monoculture, leaving you
vulnerable to blight. A reviewer whose mistakes don't line up with
yours has a chance of catching what your model can't see. So you can bring that
outside perspective into a code review, design session, or research: your agent calls
`consult`, and the subagent does its own exploration and synthesis, reporting a
summary back.

kaibo integrates as a stdio [MCP](https://modelcontextprotocol.io) server and supports
Anthropic, Gemini, DeepSeek, and any OpenAI-compatible endpoint, including local
services like llama.cpp. Each agent is set up to mix small models for exploration with
larger models for synthesis, to help keep your API spend down.

The agents reach your code through one tool: a [kaish](https://github.com/tobert/kaish)
shell. kaish has all of its commands built in and mounts your project through a
virtual filesystem layer that is read-only, so the agents can read files in your
workspace and write nowhere.

## Installation

kaibo ships as a single self-contained binary (Linux builds are fully static musl —
they run on any distro). Requires a Rust toolchain ≥ 1.85. A simple setup will get
most folks rolling. kaibo provides resources to your agent so that it can configure
kaibo for your system and credentials.

```sh
cargo install kaibo
```

<!-- TODO(repo): add the release-binary download line once the v* tag workflow has
     published artifacts — Linux x86_64/aarch64 (static musl), macOS, Windows. -->

Then register it with your agent. For **Claude Code**:

```sh
claude mcp add kaibo -- kaibo
```

or write the stdio stanza directly (`.mcp.json` in your project, or your client's MCP
config):

```jsonc
{
  "mcpServers": {
    "kaibo": {
      "command": "kaibo",
      // No args needed — an MCP client launches kaibo with cwd = your workspace, and
      // it scopes its read-only access there. Common overrides (pick what you need):
      //   "args": ["--root", "/path/to/project"]        // pin a fixed project root
      //   "args": ["--allow-path", "/extra/tree"]        // widen read scope (repeatable)
      //   "args": ["--cast", "deepseek"]                 // default cast when a call omits it
      //   "args": ["--no-run-kaish"]                     // drop a tool from the surface
      //   "args": ["--config", "/path/to/config.toml"]   // use an explicit config file
      "args": [],
      "env": {
        // The consulted models need their provider keys. Set the ones whose casts
        // you use; a missing key only matters when you actually call that cast.
        // Want secrets out of this file? Drop `env` entirely and let kaibo read them
        // from your shell environment, or point a backend at a key FILE via
        // `api_key_file` in config.toml. The TOML stores only the env-var NAME or the
        // file PATH — never the secret — so nothing sensitive lands in a config you
        // might commit or paste.
        "ANTHROPIC_API_KEY": "...",
        "DEEPSEEK_API_KEY": "...",
        "GEMINI_API_KEY": "..."
      }
    }
  }
}
```

## Configuration

The fastest way to set up is to let your agent do it. kaibo ships a **`configure`
prompt** for exactly this — in Claude Code it shows up as `/kaibo:configure` (pass an
optional goal, e.g. `/kaibo:configure a local-only privacy cast`). It walks the agent
through reading kaibo's config, asking which providers you have, and writing your
`config.toml` — keeping keys in env vars or files, never inline.

The file is written by your **host agent** (Claude Code) with its own permissions,
the same way it edits any file — *not* by kaibo's sandboxed sub-agents, which remain
strictly read-only and never touch disk. The prompt just hands your agent the
knowledge to do it.

The prompt leans on two MCP resources kaibo serves, which you can also read directly:

- **`kaibo://config/example`** — the fully annotated `config.toml` template, every
  knob with its default and a comment, embedded in the binary.
- **`kaibo://config`** — the *resolved* runtime state: which casts and backends are
  live, what's gated, where each key is sourced from.

---

## Backends, Roles, and Casts

Model diversity *is* the product, so configuration is first-class. kaibo works out of
the box with environment variables and built-in defaults, so a missing config file is
not an error. `$XDG_CONFIG_HOME/kaibo/config.toml` lets you wire your own roster. The
config has three concepts for configuring models:

- **backend** — a *connection*: which wire protocol (`anthropic` | `deepseek` |
  `gemini` | `openai`), base URL, and where its key comes from. Secrets never live in
  the TOML — only the *name* of an env var or the path to a key file.
- **role** — a *job* a model does: `explorer` (fast sweeps) and `synth` (the voice that
  answers). A slot that reads images carries a `vision` pin (see [`docs/casts.md`](docs/casts.md)).
- **cast** — a *composition*: a named team assigning models to roles. The `cast` call
  argument selects the ensemble.

```toml
# ~/.config/kaibo/config.toml

[backends.gpt]
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[backends.llama]                          # a local llama.cpp server, keyless
kind = "openai"
base_url = "http://localhost:8080/v1"
key_optional = true

# A cast that mixes model families — a different lineage per role, named as one team.
# (kaibo calls this a "chimera": local Qwen does the sweeping, hosted GPT writes
# the answer. Two families, not two flavors of one.)
[casts.mixed]
explorer = "llama/qwen2.5-coder-7b"       # cheap, local sweeps — Qwen family
synth    = "gpt/gpt-5"                     # the answer gets the big model — GPT family
```

Some default casts ship in code so kaibo runs with zero config; your `config.toml`
merges over them by name. The built-ins are:

| cast | explorer | synth |
|---|---|---|
| `anthropic` *(default)* | `claude-haiku-4-5` | `claude-sonnet-4-6` |
| `deepseek` | `deepseek-v4-flash` | `deepseek-v4-pro` |
| `gemini` | `gemini-flash-lite-latest` | `gemini-3.5-flash` |
| `openai` | local Gemma (small) | local Gemma (large) |

Per-call overrides, env vars, and CLI flags all layer over the file
(`per-call > CLI > env > file > built-in`). The full surface — per-slot thinking
budgets, effort, sampling, system-prompt overrides, house-rules injection — is in
[`docs/config.md`](docs/config.md), with a commented template in
[`docs/config.example.toml`](docs/config.example.toml); the design rationale behind the
backends/casts split is [`docs/casts.md`](docs/casts.md). The live, resolved state is
always readable at the `kaibo://config` MCP resource.

---

## Tools

Each tool is gated independently via `--no-<tool>`. All are on by default. A server with
every tool off is refused at startup.

### `consult` — hybrid agent that explores and synthesizes

Ask a model *outside your own family* about a codebase; get a grounded, cited answer.
A capable model drives a fast explorer sub-agent and can fill in any gaps using its own
`run_kaish` tool. The models have instructions to return a synthesized report at the
end, leaving the noise from the consultation out of your context. Describe what you did
or want to know in prose — kaibo reads the real, current source itself, so you don't
need to paste a diff; optionally seed it with `context` (a change summary or pasted
source), which it trusts as starting evidence while investigating for more. The answer
carries a provenance footer naming the cast and models that produced it.

When an answer surprises you, pass `include_report: true` to get the explorer's raw
findings back alongside it — the audit trail for how the consultation reached its
conclusion. (For deeper tracing, kaibo emits per-tool OpenTelemetry spans; see
[`docs/config.md`](docs/config.md).)

*Args:* `question` (required), `context` (optional seed), `path` (project dir; optional
with a default `--root`), `cast`, `session_id`, `include_report`, and per-call
`explorer_model` / `synth_model` (+ `_backend`) overrides.

### `oneshot` — a thin second opinion

The counterpart to `consult` for when you already own the context: prompt in, answer
out, no codebase access and no tools — exactly one upstream request. Use it to ask a
model outside your family a direct question (you've pasted what's needed, or it's
general). Pick the model with `cast`; the answer carries the same provenance footer.

*Args:* `prompt` (required), `cast`, and a per-call `model` (+ `backend`) override.

### `run_kaish` — direct read-only shell

Drive the read-only kaish shell yourself, no model in the loop: returns exit code +
stdout + stderr. For a Claude Code user this offers little over the built-in Bash tool
beyond *safety* — writes and external commands are refused, so exploration leaves
nothing to review: there's no diff, because nothing it runs can change your tree.

---

## How it works

```
your agent ──stdio MCP──▶ kaibo
                            │  consult(question or request)
                            ▼
                    ┌───────────────────────────┐   ┌───────────────────────────┐
                    │ synth model (capable)     │   │ explorer (lite)           │
                    │   • reads files via kaish │   │   • reads files via kaish │
                    │   • delegates to explorer │-> │                           │
                    │   • writes a summary      │ <-│   • summarizes results    │
                    └───────────────────────────┘   └───────────────────────────┘
                            │  synthesized answer (not the transcript)
                            ▼
                       back to your agent
```

kaibo is an agent for your agent. kaibo's consult() agent drives the read-only
`run_kaish` shell for working with the filesystem and transforming inputs. When using the
consult() tool, it starts with a big model, which can delegate to a fast explorer
sub-agent. The explorer/synthesis combination is meant to speed up execution and save token
spend by having smaller and faster models do the bulk of tool calling operations before
synthesis begins. (oneshot() skips all of that — a single direct call when you already
have the context.)

It is written in Rust on top of [`rmcp`](https://crates.io/crates/rmcp) and
[`rig-core`](https://crates.io/crates/rig-core). [kaish](https://crates.io/crates/kaish-kernel)
comes as a rust crate and is embedded directly in kaibo, no exec() or repl shells involved.

---

## Why not just use my agent's subagents?

Subagents spawned by your agent are the same model family. They inherit the same
correlated failures — same training, same blind spots. kaibo's consultants are usually
**different models entirely**, bringing the benefits of diversity of experience.
They're also read-only by construction, so you can point them at things where you
might not trust a read-write subagent to behave.

## FAQ

**Is read-only actually guaranteed, or best-effort?** It's structural, not best-effort.
kaibo compiles in only kaish's `localfs` axis — the `subprocess`, `git`, `host`, and
`os-integration` features are off, so `exec`/`spawn`/`git`/`ps` *don't exist in the
binary* — and mounts the project read-only on top. The
[`docs/sandbox-probes.md`](docs/sandbox-probes.md) runbook is how we live-test that
boundary — write/external-command/read-escape batteries run against the shipped binary.

**How long does a consult take?** It's a multi-step investigation, not a single API
call — a deep one can run a few minutes, more with thinking on and a large repo to
sweep. kaibo emits MCP progress notifications as the explorer and synth work, so a
client that surfaces them shows live progress; whether you actually see those beats is
up to your agent's UI, which kaibo can't control.

**Can a runaway consultation melt my machine or my budget?** There are hard ceilings
on both. Every kaish script is capped at 30s wall-clock, 64 KiB of output, and 64 MB of
in-memory scratch (a write past the cap fails loudly rather than growing without
bound), so a `while true; grep -r /` can't run away. The model loops are bounded too:
the explorer sweep and the consult driver stop at a turn limit (100 and 200 by
default), so a confused model can't loop forever burning tokens. All of these are
configurable in `config.toml`.

**What providers are supported?** Anthropic, DeepSeek, and Gemini natively, plus a
generic `openai` kind for any OpenAI-compatible endpoint (hosted GPT, a local
llama.cpp / Ollama / Gemma server, …). See [Backends, Roles, and Casts](#backends-roles-and-casts).

**Does it need network or credentials?** The consulted models do — their provider
APIs, with keys you supply. kaish itself reaches nothing: no network, no credentials.
Telemetry is off by default and, when on, only opens an *outbound* OTLP connection
you configure.

**What happens if a provider is overloaded or down (a 429/529, a reset, a wedged
backend)?** kaibo does **not** retry — a single completion is bounded by the backend's
`request_timeout` (default 15 min, the wall-clock for one model call) and a ≤10s
`connect_timeout` that fails a dead endpoint fast. When a provider fails, the consult
returns a **clean tool-result error** (`is_error`) naming the cast and the underlying
detail, rather than a protocol-level error — because a consult is an *optional* second
opinion, so your agent should read "the consult failed, here's why" and proceed without
it (or call again), not have its own turn fail. The message is **tailored to the
failure**: a transient condition (overload, rate-limit, timeout, reset) says so and
invites you to retry the call, so the calling agent can drive the retry; a non-transient
error (auth, bad request) doesn't, since a retry won't help; and a kaibo-side failure is
named as such rather than blamed on the provider. If a provider is reliably slow, raise
that backend's `request_timeout_secs`. (Automatic retry/backoff belongs in the HTTP layer
— tracked as an upstream `rig` contribution in [`docs/issues.md`](docs/issues.md).)

**What's the cost?** `consult` spends tokens on the provider behind the chosen cast. A
family-mixing cast (cheap local explorer + hosted synth) keeps the broad, token-heavy
sweeping cheap and pays the strong model only for the answer. The agent conversations
are set up so they can cache well on most providers.

---

## Contributing

Agent contributions welcome. Every change lands through a pull request — for
transparency, so anyone using kaibo can see what changed and why — and every
user-facing change gets a [`CHANGELOG.md`](CHANGELOG.md) entry. See
[`AGENTS.md`](AGENTS.md) for the architecture, the PR-and-changelog workflow, and
working conventions, [`docs/issues.md`](docs/issues.md) for the live tracker of open
work, and [`docs/devlog.md`](docs/devlog.md) for the shipped-work record.

## Name

Originally @tobert was combining 'kai' (会) with different words to find a name for this
tool. kai + [aibo](https://jisho.org/word/%E7%9B%B8%E6%A3%92) sounded nice to align with
[gpal](https://github.com/tobert/gpal)/[dpal](https://github.com/tobert/dpal)/[cpal](https://github.com/tobert/cpal).
It turns out kaibo ([解剖](https://jisho.org/word/%E8%A7%A3%E5%89%96)) means
dissection, autopsy, or postmortem examination and that was that.

## License

MIT
