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
Anthropic, Gemini, DeepSeek, OpenRouter (one key reaching every major model family),
and any OpenAI-compatible endpoint, including local services like llama.cpp. It's a
tool your *agent* uses — any MCP-capable client can drive it — and it's also a
[CLI](#cli) for scripts, CI, and terminals that would rather shell out than speak
MCP. Each agent is set up to mix small models for exploration with larger models
for synthesis, to help keep your API spend down.

The agents reach your code through one tool: a [kaish](https://github.com/tobert/kaish)
shell. kaish has all of its commands built in and mounts your project through a
virtual filesystem layer that is read-only, so the agents can read files in your
workspace and write nowhere.

## What it looks like

Your agent just reworked some concurrency-sensitive code and wants eyes from
outside its own family. It calls:

```jsonc
consult({
  "question": "We reworked job_wait to park instead of busy-polling — the return
    trigger is now any Warn+ record or a clean timeout. Review the implementation:
    does anything still return early at sub-warn levels or busy-poll? Cite spans.",
  "cast": "deepseek"
})
```

kaibo's explorer sweeps the repo through the read-only shell, the synth reads the
hot spans itself, and one synthesized review comes back — cited, with the
investigation noise left out of your context:

> **No.** The `absorb` closure is the sole return gate, and it's correct
> (`src/mcp_log.rs:250-267`): `woke` is only set when a record meets `wake_floor` —
> rank(Warning) — and the handler hardcodes that floor (`src/server/mod.rs:2499`);
> no caller input can change it. […] The drain→park race is handled: `notify_one()`
> stores a permit when no task is waiting, so a push that races a `job_wait`'s
> registration isn't missed (`src/mcp_log.rs:271-286`). […] The only real tension
> is the unconditional `notify_one()` in `push` (`src/mcp_log.rs:166`) — every
> record, even Debug noise, briefly wakes the parked loop. An optimization concern,
> not a correctness one. *[abridged]*
>
> ——— kaibo · cast `deepseek` · explorer `deepseek-v4-flash` · synth `deepseek-v4-pro`

That's a real consult against this repo. It ran about four minutes and cost
**$0.02** (measured by account-balance delta; DeepSeek's prompt cache was warm — a
cold first run costs a few cents more). The full answer also surfaced a benign
spurious-wakeup path and an optimization wrinkle we hadn't written down anywhere —
which is the point: a model that didn't make your model's mistakes, reading your
real source, reporting back one summary.

## Installation

kaibo ships as a single self-contained binary per platform — download it, check the
checksum, put it on your `PATH`, done. Linux builds are fully static musl and run on
any distro; macOS (Apple silicon and Intel) and Windows binaries are self-contained
too. Grab your platform's archive and its `.sha256` from the
[releases page](https://github.com/tobert/kaibo/releases):

```sh
sha256sum -c kaibo-*-x86_64-unknown-linux-musl.tar.gz.sha256
tar xzf kaibo-*-x86_64-unknown-linux-musl.tar.gz
install -m 0755 kaibo ~/.local/bin/    # anywhere on your PATH
```

Prefer building from source? `git clone https://github.com/tobert/kaibo` and
`cargo install --path kaibo` with a Rust toolchain ≥ 1.85 produces the same binary.

### Verify a download

Every release is born signed in public CI: the signing identity *is* the release
workflow at that tag, witnessed by the Sigstore transparency log — no maintainer
key to steal or trust. Two independent checks; either one is sufficient.

With the [`gh` CLI](https://cli.github.com/), SLSA build provenance is one command
against any file from the release:

```sh
gh attestation verify kaibo-v0.2.0-x86_64-unknown-linux-musl.tar.gz -R tobert/kaibo
```

With [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) ≥ 3
(no GitHub tooling needed), verify the signed checksum manifest once and it
covers every file it lists. Grab `checksums.txt` and `checksums.txt.sigstore.json`
from the release, substituting the tag you downloaded in the identity:

```sh
cosign verify-blob \
  --bundle checksums.txt.sigstore.json \
  --certificate-identity "https://github.com/tobert/kaibo/.github/workflows/release.yml@refs/tags/vX.Y.Z" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  checksums.txt
sha256sum -c --ignore-missing checksums.txt
```

The bundle is self-contained — certificate, signature, and transparency-log
entry in one file — so that verify works offline.

Each release also carries an SPDX SBOM (`kaibo-<tag>-sbom.spdx.json`) cataloging
the exact locked dependency tree the binaries were built from.

### Container image (ghcr)

The same release ships as a multiarch (amd64/arm64) container image,
[`ghcr.io/tobert/kaibo`](https://github.com/tobert/kaibo/pkgs/container/kaibo) — the fully-static binary
in a distroless, shell-less, non-root base.

Because the binary links against nothing, the image doubles as a **COPY
source** — the easiest way to put kaibo into a devcontainer (or any image),
with zero libc or package-manager concerns:

```dockerfile
COPY --from=ghcr.io/tobert/kaibo:latest /usr/local/bin/kaibo /usr/local/bin/kaibo
```

That's the whole install: inside a devcontainer your workspace mount is already
declared, so kaibo registers as a plain local MCP server from there.

To run the image itself as the MCP server, mount your project read-only at
`/work` (the image's working directory — kaibo scopes its read access to it)
and keep stdin open:

```sh
claude mcp add kaibo -- docker run --rm -i \
  -u "$(id -u):$(id -g)" \
  -v "$PWD:/work:ro" \
  -e DEEPSEEK_API_KEY \
  ghcr.io/tobert/kaibo:latest
```

- **`-i` is load-bearing.** kaibo is a stdio MCP server: without `-i`, stdin
  closes immediately and the container exits 0 in silence — and distroless has
  no shell to debug into.
- `-u` runs the container as you so the mount's file permissions line up;
  podman users want `--userns=keep-id` in its place.
- Pass provider keys by name (`-e DEEPSEEK_API_KEY`) — the value rides your
  shell environment, never your MCP config.
- The `:ro` mount is an OS-enforced belt under kaibo's own read-only sandbox:
  kaibo never writes either way, this just makes the kernel agree.

The same image is a [CLI](#cli) too — append a subcommand and drop `-i` (there's no
stdio transport to hold open for a one-shot run):

```sh
docker run --rm -v "$PWD:/work:ro" -e DEEPSEEK_API_KEY \
  ghcr.io/tobert/kaibo:latest consult "what does this project do?" --cast deepseek
```

The package is public — pulling (and `COPY --from`) needs no registry login.
The image is signed and attested by the same machinery as the archives — verify
with `gh attestation verify oci://ghcr.io/tobert/kaibo:<tag> -R tobert/kaibo`,
or `cosign verify ghcr.io/tobert/kaibo:<tag>` with the same two identity flags
shown above.

Then register it with your agent. kaibo is a standard stdio MCP server, so any
MCP-capable client works — Claude Code, Codex CLI, Cline, OpenCode, whatever you
run. The stanza goes in your client's MCP config; in Claude Code that's `.mcp.json`
in your project, and `claude mcp add kaibo -- kaibo` writes the same registration
for you:

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
      // The consulted models need provider keys — but don't put them here; this
      // file tends to get committed. Leave `env` empty and kaibo reads keys from
      // your shell environment (ANTHROPIC_API_KEY, DEEPSEEK_API_KEY,
      // GEMINI_API_KEY, OPENROUTER_API_KEY, …), or point a backend at a key FILE
      // via `api_key_file` in config.toml. Config stores only the env-var NAME or
      // the file PATH — never the secret. A missing key only matters when you
      // actually call a cast that needs it.
      "env": {}
    }
  }
}
```

## CLI

Every tool below is also a command — no MCP client needed, so scripts, CI, and a
human at a terminal can drive kaibo directly:

| MCP tool | CLI equivalent |
|---|---|
| `consult` | `kaibo consult "question" [--cast … --attach … --session … --json]` |
| `oneshot` | `kaibo oneshot "prompt" [--attach … --json]` (context also via stdin: `oneshot "review this" < diff.txt`) |
| `explore` | `kaibo explore "question" [--cast … --json]` |
| `run_kaish` | `kaibo kaish -c 'script'` |
| `batch_submit`, `job_get`/`job_list` (batch handles) | `kaibo batch submit \| get \| list` |
| `kaibo://config` resource | `kaibo config` |

```sh
kaibo consult "does anything still busy-poll in job_wait?" --cast deepseek
```

**Bare `kaibo` (no subcommand) is still the MCP server** — every existing client
config keeps working unchanged; `kaibo serve` is the explicit spelling for the
same thing. `consult_submit`/`job_wait` and `deliberate`'s direct lane are the
one gap — every other tool in the table above has a CLI subcommand; queued as
[#82](https://github.com/tobert/kaibo/issues/82).

**stdout is the answer, stderr is everything else.** Progress, logs, and warnings
go to stderr, so piping stays clean; the answer (with the same provenance footer
the MCP tool appends) is the only thing on stdout. `--json` swaps that for a
structured envelope (`{answer, cast, models, usage, warnings}`) for a script
caller — its `answer` field is always the model's raw words, never a kaibo notice.

**Exit codes have teeth**, so a caller branches on the code instead of parsing
prose: `0` an answer, `2` a usage error (bad flag, unknown or wrong-for-the-tool
cast), `3` a setup/containment rejection (a path outside the allowed set, a
missing provider key), `4` the work ran and failed at runtime (a provider
error, a model-loop failure). `kaibo kaish` is the one exception — it passes
through kaish's own exit code (`0` ok, `126` blocked, `124` timed out) instead,
since a script branches on *that* to know what the sandboxed command did.

The shared flags (`--root`, `--allow-path`, `--cast`, `--config`, house-rules
files, …) work before or after the subcommand and are documented in `kaibo
--help`; each subcommand's own flags are in `kaibo <subcommand> --help`.

**State: where `--session` lives.** A `--session NAME` thread (and every
`batch_submit`/`kaibo batch submit` handle) is durable by default, in a small
[turso](https://github.com/tursodatabase/turso) db at a fixed path kaibo picks —
`$XDG_STATE_HOME/kaibo/state.db` (`~/.local/state/kaibo/state.db` when unset) —
never inside your project and never a path a model controls. It's what lets a
session started over MCP continue on the CLI and back, and a batch handle survive
a restart. It stores only the lean `(question, answer)` turns of sessions you
name and each batch's `{backend, provider-id}` — never anything else, and never a
source of truth: safe to delete, or move it with `--state-db FILE`
(`KAIBO_STATE_DB`), or skip it for one run with `--no-persistence`
(`KAIBO_NO_PERSISTENCE`) to go fully in-memory. See
[Persistence](docs/config.md#persistence-persistence) for the full contract
(what's excluded, the fail-loud-on-a-bad-path behavior, the one Windows carve-out).

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

Prompts and resources are plain MCP — any client that surfaces them can use them. If
yours doesn't render `configure` as a command, read `kaibo://config/example`
directly and write the TOML yourself.

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

### `explore` — the survey, not the verdict

The same read-only investigation as `consult`, returning a structured, cited survey
report — what's where, how it fits together — instead of an answer. Reach for it
when you want the map.

### `consult_submit` + `job_wait` / `job_get` / `job_list` / `job_cancel` — async

A deep consult can run minutes; `consult_submit` runs the same investigation in the
background and hands back a job handle immediately, so your agent keeps working.
`job_wait` parks until something finishes; `job_get` collects the answer.

### `batch_submit` — frontier answers at half price

`oneshot`'s async sibling: fan self-contained prompts to a top-tier model on the
provider's batch lane — offline, maximum thinking, roughly half the interactive
price. The built-in `anthropic-batch` and `gemini-batch` casts put Claude Opus or
Gemini Pro on your hardest questions cheaply; handles are durable across restarts.

### `deliberate` — deep offline reasoning over a dossier

An investigation assembles a cited dossier from your repo, then a frontier (or big
local) model reasons over it offline on the batch lane — depth over speed, at batch
prices.

### `run_kaish` — direct read-only shell

Drive the read-only kaish shell from your agent, no model in the loop: returns exit
code + stdout + stderr. For a Claude Code user this offers little over the built-in
Bash tool beyond *safety* — writes and external commands are refused, so exploration
leaves nothing to review: there's no diff, because nothing it runs can change your tree.

---

## Backends, Roles, and Casts

Short version: a **cast** is just a named team — a cheap explorer that sweeps plus a
strong synth that answers — and a call picks its team with the `cast` argument.
Everything below is how you wire your own.

Model diversity *is* the product, so configuration is first-class. kaibo works out of
the box with environment variables and built-in defaults, so a missing config file is
not an error. `$XDG_CONFIG_HOME/kaibo/config.toml` lets you wire your own roster. The
config has three concepts for configuring models:

- **backend** — a *connection*: which wire protocol (`anthropic` | `deepseek` |
  `gemini` | `openrouter` | `openai`), base URL, and where its key comes from. Secrets
  never live in the TOML — only the *name* of an env var or the path to a key file.
  `openrouter` is a keyed gateway with a fixed endpoint — one key reaching every major
  model family, reasoning on by default via its unified `effort` param.
- **role** — a *job* a model does: `explorer` (fast sweeps) and `synth` (the voice that
  answers). A slot that reads images carries a `vision` pin (see [`docs/casts.md`](docs/casts.md)).
- **cast** — a *composition*: a named team assigning models to roles. The `cast` call
  argument selects the ensemble; the calling agent sees these names in its tool listing,
  so a descriptive name (`local-only`, `deep-dive`) lets it pick a team by intent without
  reading your config.

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
| `openrouter` | `qwen/qwen3.6-flash` | `qwen/qwen3.7-max` |
| `openai-local` | local Gemma (small) | local Gemma (large) |
| `anthropic-batch` | — | `claude-opus-4-8` (batch lane) |
| `gemini-batch` | — | `gemini-pro-latest` (batch lane) |

`openai-local` points at *your* OpenAI-compatible server on localhost (llama.cpp,
Ollama, …) — you run the model server; kaibo ships no inference of its own. The
`*-batch` casts staff the offline lane for `batch_submit` and `deliberate`. And the
`anthropic` default is just the zero-config bring-up team — the whole point is a
reviewer *outside your agent's family*, so if your agent is a Claude, reach for
`--cast deepseek` or `--cast gemini`; if it runs on DeepSeek or GPT, go the other way.

Per-call overrides, env vars, and CLI flags all layer over the file
(`per-call > CLI > env > file > built-in`). The full surface — per-slot thinking
budgets, effort, sampling, system-prompt overrides, house-rules injection — is in
[`docs/config.md`](docs/config.md), with a commented template in
[`docs/config.example.toml`](docs/config.example.toml); the design rationale behind the
backends/casts split is [`docs/casts.md`](docs/casts.md). The live, resolved state is
always readable at the `kaibo://config` MCP resource.

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
binary* — and mounts the project read-only on top. The same structure bounds the
network: kaish itself reaches nothing — no sockets, no subprocesses, no credentials;
only kaibo's provider clients dial out, to the model APIs you configure. The
[`docs/sandbox-probes.md`](docs/sandbox-probes.md) runbook is how we live-test that
boundary — write/external-command/read-escape batteries run against the shipped binary.

**How long does a consult take?** It's a multi-step investigation, not a single API
call — a deep one can run a few minutes, more with thinking on and a large repo to
sweep. kaibo emits MCP progress notifications as the explorer and synth work, so a
client that surfaces them shows live progress; whether you actually see those beats is
up to your agent's UI, which kaibo can't control. If you'd rather not block on it,
`consult_submit` starts the same investigation in the background and `job_get` picks
up the answer.

**Can a runaway consultation melt my machine or my budget?** There are hard ceilings
on both. Every kaish script is capped at 30s wall-clock, 64 KiB of output, and 64 MB of
in-memory scratch (a write past the cap fails loudly rather than growing without
bound), so a `while true; grep -r /` can't run away. The model loops are bounded too:
the explorer sweep and the consult driver stop at a turn limit (100 and 200 by
default), so a confused model can't loop forever burning tokens. All of these are
configurable in `config.toml`.

**What providers are supported?** Anthropic, DeepSeek, and Gemini natively; OpenRouter
as a keyed gateway that reaches every major model family through one key (`~author/
family-latest` aliases — available for the major model authors — keep the built-in
cast current as new models ship); and a generic `openai` kind for any
OpenAI-compatible endpoint: hosted GPT, Moonshot/Kimi, Zhipu/GLM, a local
llama.cpp / Ollama server, your org's internal gateway, ….
See [Backends, Roles, and Casts](#backends-roles-and-casts).

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

**What's the cost?** `consult` spends tokens on the provider behind the chosen cast.
A real reference point: the consult in [What it looks like](#what-it-looks-like) — a
multi-minute investigation of this repo on the `deepseek` cast — cost **$0.02**,
measured by account-balance delta (cache-warm; a cold run costs a few cents more).
A family-mixing cast (cheap local explorer + hosted synth) keeps the broad,
token-heavy sweeping cheap and pays the strong model only for the answer, and the
agent conversations are set up to cache well on most providers — that caching is
where numbers like two cents come from. For the strongest models, `batch_submit`
rides the provider's batch lane at roughly half the interactive price.

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
