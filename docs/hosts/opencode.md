# kaibo in OpenCode

Checked against OpenCode 2.2.5 (released 2026-10-30), its V2 MCP docs, and its source on
2026-11-02. OpenCode still reads the older V1 shape (servers directly under `mcp`); the
entries below use V2.

## stdio (the default)

```jsonc
// opencode.json in the project, or ~/.config/opencode/opencode.json
{
  "mcp": {
    "servers": {
      "kaibo": {
        "type": "local",
        "command": ["kaibo"],
        "cwd": ".",
        "codemode": false
      }
    }
  }
}
```

- `"cwd": "."` starts kaibo in the workspace, which kaibo adopts as its scope. A relative
  `cwd` resolves from the workspace directory. In the global file, name a fixed workspace
  instead: `"command": ["kaibo", "--root", "/home/you/src"]`.
- `codemode: false` lists kaibo's tools as native tools. With Code Mode on (the default)
  the model reaches them only from code it writes.
- kaibo inherits OpenCode's environment, plus anything in `environment`.
- Timeouts: startup 30 seconds, tool listing 30 seconds, a call 12 hours
  (`"timeout": { "startup": 30000, "catalog": 30000, "execution": 43200000 }`).

## HTTP (preview)

One shared kaibo, set up with `kaibo://config/http` (`kaibo http-guide`), then:

```json
{
  "mcp": {
    "servers": {
      "kaibo": {
        "type": "remote",
        "url": "http://127.0.0.1:52426/mcp",
        "headers": { "Authorization": "Bearer {file:~/.local/state/kaibo/http-token}" },
        "oauth": false,
        "codemode": false
      }
    }
  }
}
```

- OpenCode reads `{file:...}` when it loads its config, so after a rotation, touch the
  config file or restart OpenCode. If you set `XDG_STATE_HOME`, write that path instead
  (`kaibo http-token` prints it).
- `oauth: false` keeps a `401` from starting an OAuth flow.

## Permissions

Rules name kaibo's tools as `kaibo_<tool>`; `"kaibo_*": "deny"` hides all of them. Rule
values are `allow`, `ask`, and `deny`, the last matching rule wins, and an agent's rules
override the global ones.

## What OpenCode supports

- **Resources:** through its `list_mcp_resources` and `read_mcp_resource` tools, templates
  included. `kaibo://configure` is the setup walkthrough.
- **Prompts:** as slash commands, `/kaibo:configure`.
- **Server instructions:** added to the system prompt, unless permissions deny every
  kaibo tool.
- No sandbox applies to local MCP servers.
