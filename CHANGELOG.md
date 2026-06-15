# Changelog

All notable, user-facing changes to kaibo are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kaibo aims for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`0.2.0` is the first tracked release — the point kaibo adopts a pull-request
workflow and a maintained changelog. It captures the feature set as kaibo goes
public rather than reconstructing the 0.1 development line; that history lives in
the git log. Each later release appends a new section at the top.

## [0.2.0] — unreleased

### Added

- **`consult`** — the headline tool: ask a question about a codebase, get a
  grounded, cited answer. A capable model reads precise spans directly and delegates
  broad sweeps to a cheap explorer sub-agent, then synthesizes — so your context
  receives the answer, not the investigation transcript. Args: `question`, `path`,
  `cast`, `session_id`, `include_report`, and per-call `explorer_model` /
  `synth_model` (+ `_backend`) overrides.
- **`explore`** — a fast, cheap model sweeps the project read-only and returns a
  curated report (relevant files, line numbers, key snippets) for a stronger model
  to reason over.
- **`synthesize`** — a capable model answers grounded in optional caller `context`
  (an explore report or pasted source), investigating directly when context is thin.
- **`run_kaish`** — drive the read-only kaish shell yourself, no model in the loop:
  exit code + stdout + stderr.
- **`generate_image`** — kaibo's first *capability* (an artifact handed back to the
  caller, not reasoning run into kaibo's own models): prompt → image, returned inline
  as MCP image content. OpenAI-compatible image backends only (hosted
  `gpt-image` / DALL·E, or a local Stable-Diffusion server).
- **`view_image`** — vision-capable consultation phases can read an image *file* from
  the workspace into model context (screenshots, diagrams, assets already in the tree).
- **Multi-provider model teams.** Anthropic, DeepSeek, and Gemini natively, plus a
  generic `openai` kind for any OpenAI-compatible endpoint (hosted GPT, local
  llama.cpp / Ollama / Gemma). Configured as **backends** (connections), **casts**
  (named teams), and **roles** (explorer / synth / image); a cast can mix families
  across roles — a cheap local explorer with a hosted synth. Built-in casts ship so
  kaibo runs with zero config; `config.toml` merges over them. Precedence:
  per-call > CLI > env > file > built-in, and a missing config file is not an error.
- **Guided setup.** A built-in `configure` MCP prompt walks your host agent through
  writing `config.toml`, alongside `kaibo://config` (resolved runtime state) and
  `kaibo://config/example` (annotated template) resources. Secrets are referenced by
  env-var name or key-file path, never inlined.
- **Zero-config workspace root.** When no `--root` is set, kaibo adopts its launch
  cwd as the inferred default root (it already scoped containment to that cwd, and
  MCP clients start stdio servers with cwd = workspace), so a call may omit `path`
  and still land on the project. The scope handshake and `kaibo://config` tag the
  root as inferred. An `--allow-path` that excludes the cwd leaves no default root —
  kaibo never defaults to a path its own containment check would reject.
- **Per-tool gating.** Each tool has a `--no-<tool>` flag (all on by default); an
  all-off server is refused at startup.
- **Operator ignore files** via a `[kaish.ignore]` config stanza.
- **Thinking on by default,** with model-aware request shaping (per-provider thinking
  config, per-role reasoning effort, generous completion-token headroom).
- **Multi-turn sessions** via `session_id`, and optional OTLP/HTTP trace export
  (`[telemetry]`, off by default).
- **Single self-contained binary** per platform; Linux builds are fully static
  (musl). TLS is rustls + ring — no OpenSSL, no aws-lc, no C toolchain.

### Security

- **Read-only is structural, not best-effort.** kaibo compiles in only kaish's
  `localfs` axis — `subprocess` / `git` / `host` / `os-integration` are off, so
  `exec` / `spawn` / `git` / `ps` don't exist in the binary — and mounts the project
  read-only, with an in-memory scratch filesystem for everything else. Reads are
  scope-bounded to `--root` / `--allow-path` (launch cwd by default), enforced after
  symlink and `..` canonicalization.
- **Bounded resource use.** Each kaish script is capped (30 s wall-clock, 8 KB
  output, 64 MB scratch — over-cap fails loudly, never a silent drop), and the model
  loops stop at turn limits, so a runaway consultation can't melt the machine or the
  budget. All configurable.

[0.2.0]: https://github.com/tobert/kaibo/releases/tag/v0.2.0
