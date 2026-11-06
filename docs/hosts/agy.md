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

## HTTP (preview)

One shared kaibo, set up with `kaibo://config/http` (`kaibo http-guide`), then:

```sh
agy mcp add --header "Authorization: Bearer $(kaibo http-token --print)" \
  kaibo http://127.0.0.1:52426/mcp
```

The entry gets `serverUrl` and `headers`. agy has no header helper, so the token is stored
in `mcp_config.json`. After a rotation, run the same command again (`agy mcp add` updates
an existing entry) and restart agy.

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
