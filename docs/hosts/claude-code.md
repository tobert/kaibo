# kaibo in Claude Code

Checked against Claude Code's MCP, permissions, and sandboxing docs (releases through
v2.1.307) on 2026-11-02. Confirm against `claude --version` before copying.

## stdio (the default)

```sh
claude mcp add --scope user kaibo -- kaibo
```

User scope loads kaibo in every project. Claude Code starts a stdio server in the project
directory, and kaibo adopts that directory as its scope, so no `--root` is needed. Add one
when you want a fixed workspace: `claude mcp add --scope user kaibo -- kaibo --root ~/src`.

The entry lands in `~/.claude.json`. A project-scoped entry goes in `.mcp.json`:

```json
{ "mcpServers": { "kaibo": { "command": "kaibo", "args": [] } } }
```

A project `.mcp.json` server needs your approval the first time; `claude -p` loads it
without asking.

## HTTP (preview)

One shared kaibo, set up with `kaibo://config/http` (`kaibo http-guide`), then:

```jsonc
// ~/.claude.json, user scope
{
  "mcpServers": {
    "kaibo": {
      "type": "http",
      "url": "http://127.0.0.1:52426/mcp",
      "headersHelper": "kaibo http-token --header",
      "timeout": 600000
    }
  }
}
```

- `headersHelper` runs at session start, on every reconnect, and once more after a `401`,
  so a rotated token is picked up with no restart. It has 10 seconds.
- At project or local scope, the helper runs only after you accept the folder trust
  prompt; user scope has no such gate.
- An HTTP request times out after 60 seconds unless `timeout` (or `MCP_TOOL_TIMEOUT`)
  raises it. A `consult` often runs longer.

## Permissions

Rules name kaibo's tools as `mcp__kaibo__<tool>`:

```json
{ "permissions": { "allow": ["mcp__kaibo__consult", "mcp__kaibo__list_models"] } }
```

`mcp__kaibo` or `mcp__kaibo__*` matches every kaibo tool.

## Sandbox and timeouts

- Claude Code's sandbox covers Bash and its child processes. MCP servers run outside it,
  so kaibo reaches the model APIs even when Bash has no network.
- `kaibo consult` run from a sandboxed Bash needs network access to the provider hosts and
  write access to kaibo's state and CAS directories.
- `MCP_TIMEOUT` bounds startup (default 30 seconds). A stdio tool call that sends nothing
  for 30 minutes is aborted (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`); for long work, use
  `consult_submit` and `job_wait`.

## What Claude Code supports

- **Resources:** `@` mentions, plus tools to list and read them. `kaibo://configure` is the
  setup walkthrough.
- **Prompts:** as slash commands, `/kaibo:configure`.
- **Server instructions:** loaded at session start.
- **Tool descriptions:** cut at 4,096 characters when loaded up front, 16,384 when loaded
  through tool search. kaibo pins `consult` up front with `_meta["anthropic/alwaysLoad"]`.
