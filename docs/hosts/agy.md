# kaibo in Antigravity CLI (`agy`)

Checked against agy 1.4.9 (`agy mcp --help`, `agy changelog`, and the files `agy mcp add`
writes) on 2026-11-02. agy replaces the retired Gemini CLI.

## stdio (the default)

```sh
agy mcp add kaibo -- kaibo --root ~/src
```

That writes `~/.gemini/config/mcp_config.json`:

```json
{
  "mcpServers": {
    "kaibo": {
      "command": "kaibo",
      "args": ["--root", "/home/you/src"],
      "disabled": false
    }
  }
}
```

- Flags go before the server name, and `--` comes before a command or arguments that
  start with `-`.
- The entry also takes `env`, `cwd`, `enabledTools`, and `disabledTools`.
- Which directory agy starts a stdio server in is not documented, so name the scope with
  `--root`.

## Unix socket (preview)

No config entry to copy yet. As of 2026-11-22 this guide has not confirmed that agy can
connect to a Unix socket: its documented HTTP transport takes a URL. Use stdio above, with `--connect` in the args to forward to a running
daemon (`kaibo://config/http`, "Reach the daemon from a stdio host"). When
an agy release can dial a socket path, point it at the path `kaibo://config` shows under
`[http]`.

## Permissions

Allow rules in `~/.gemini/antigravity-cli/settings.json` name kaibo's tools as
`mcp(kaibo/<tool>)`:

```json
{ "permissions": { "allow": ["mcp(kaibo/consult)", "mcp(kaibo/list_models)"] } }
```

## What agy supports

- **Resources:** read through agy's `read_resource`. `kaibo://configure` is the setup
  walkthrough.
- **Prompts:** not confirmed. Read `kaibo://configure` instead of the `configure` prompt.
- **Server instructions:** agy caches them, with the tool schemas, under
  `~/.gemini/antigravity-cli/mcp/kaibo/`.
- `agy --sandbox` restricts the terminal; its effect on MCP servers is not documented.
