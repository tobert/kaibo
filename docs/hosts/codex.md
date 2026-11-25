# kaibo in Codex

Checked against Codex CLI 0.166.0 (released 2026-10-29) and its MCP and config reference
docs on 2026-11-02. Confirm with `codex --version` before copying.

## stdio (the default)

```toml
# ~/.codex/config.toml
[mcp_servers.kaibo]
command = "/absolute/path/to/kaibo"
args = ["--root", "/absolute/path/to/project"]
tool_timeout_sec = 600
default_tools_approval_mode = "prompt"

[mcp_servers.kaibo.tools.list_models]
approval_mode = "approve"
```

- **Codex clears the server's environment.** It passes only `HOME`, `LOGNAME`, `PATH`,
  `SHELL`, `USER`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, and `TZ`, plus what you list. If
  you set `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, or `XDG_DATA_HOME`, or a key that an
  `api_key_env` names, list it in `env_vars` (`env_vars = ["XDG_STATE_HOME"]`), or kaibo
  reads its defaults under `~/.config` and `~/.local`. `api_key_file` needs no forwarding.
- **Timeouts:** a tool call times out after 60 seconds by default (`tool_timeout_sec`),
  and startup after 10 (`startup_timeout_sec`). A `consult` often runs longer: raise
  `tool_timeout_sec`, or use `consult_submit`, then `job_wait` with a `timeout_secs` below
  the tool timeout. A client timeout does not stop the call at the provider.
- **Approval:** `default_tools_approval_mode` (`auto`, `prompt`, `writes`, `approve`) sets
  every kaibo tool; a per-tool `approval_mode` wins. Approval does not cover `kaibo` run
  from Codex's shell.
- `required = true` makes Codex fail to start when kaibo cannot; the default carries on
  without it.

## Unix socket (preview)

No config entry to copy yet. As of 2026-11-22 this guide has not confirmed that Codex can
connect to a Unix socket: its documented HTTP transport takes a URL. Use stdio above, with `--connect` in the args to forward to a running
daemon (`kaibo://config/http`, "Reach the daemon from a stdio host"). When
a Codex release can dial a socket path, point it at the path `kaibo://config` shows under
`[http]`.

## Sandbox

Codex's command sandbox applies to shell commands, not to MCP servers. So the kaibo MCP
tools can reach a provider while `kaibo consult` run from Codex's shell fails with a
connection error. Use the MCP tools, or give the command sandbox network access to the
provider hosts. Shell-run `kaibo` also needs write access to its state directory
(`~/.local/state/kaibo`, including the database's sidecar files) and CAS directory
(`~/.local/share/kaibo/cas`); `--state-db` and `--cas-dir` give Codex its own, and
`--no-persistence` gives up durable sessions. A wrapper or container around Codex can
still limit the MCP server.

## What Codex supports

- **Resources and resource templates:** yes, through tools the model calls.
  `kaibo://configure` is the setup walkthrough.
- **Prompts:** no. Read `kaibo://configure` instead of the `configure` prompt.
- **Server instructions:** shown to the model as the description of kaibo's tools.
- After changing launch settings, restart Codex, then read `kaibo://config` to confirm
  `allowed_paths`, the default cast, and key sources.
