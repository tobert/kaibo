# kaibo in Claude Desktop

Checked against the MCP local-server quickstart and Anthropic's connector docs on
2026-11-02.

## stdio (the only route)

Open Settings, then Developer, then Edit Config, and add:

```json
{
  "mcpServers": {
    "kaibo": {
      "command": "/absolute/path/to/kaibo",
      "args": ["--root", "/absolute/path/to/workspace"]
    }
  }
}
```

- The file is `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS
  and `%APPDATA%\Claude\claude_desktop_config.json` on Windows.
- Use absolute paths for the command and for `--root`. Desktop's launch directory is not
  documented, and kaibo does not adopt `~` or `/` as its scope, so without `--root` every
  call must pass `path`.
- Provider keys: `api_key_file` or `api_key_cmd` in kaibo's `config.toml`, or `env` in
  this entry.
- Logs: `~/Library/Logs/Claude/mcp*.log` on macOS, `%APPDATA%\Claude\logs` on Windows.
- Restart Desktop after changing the entry.

## HTTP

Not from Desktop. Custom connectors connect from Anthropic's cloud, so they cannot reach a
loopback address such as kaibo's HTTP server. Use stdio.

## What Desktop supports

- **Tool approval:** each call asks, with an "Always allow" choice; tools can be turned off
  per connector.
- **Resources and prompts:** supported. `kaibo://configure` is the setup walkthrough.
- **Server instructions:** not shown to the model, so each kaibo tool description stands
  on its own.
