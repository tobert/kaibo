# kaibo over HTTP (preview)

kaibo serves MCP over stdio by default. The HTTP transport serves the same tools from one
long-lived process on a loopback socket. Use it when a host's sandbox makes launching a
process hard, or to share one kaibo, with one workspace and one set of provider keys,
across several agents on one machine.

It is a preview: off by default, loopback only, one bearer token. Every token holder is
treated as the operator, so sessions, jobs, and `kaibo://config` are shared by every
connected client.

## Turn it on

```toml
# ~/.config/kaibo/config.toml
[server]
root = "~/src"

[server.http]
enabled = true
# listen = "127.0.0.1:52426"   # the default; loopback only
```

Then run `kaibo serve`. The same switches exist as `kaibo serve --http --listen ADDR` and
`KAIBO_HTTP=1` / `KAIBO_HTTP_LISTEN`. `kaibo serve --no-http` or `KAIBO_HTTP=0` serves
stdio for one run without editing the file. The startup log prints the URL,
`http://127.0.0.1:52426/mcp`.

A listen address off loopback is refused at startup. To reach kaibo from another machine,
forward the port over SSH: `ssh -L 52426:127.0.0.1:52426 host`.

## Choose the scope

Every file in scope may reach a configured model. Scope the server to a workspace, the
directory that holds your projects, and keep private files out of reach:

```toml
[server]
root = "~/src"
allow_paths = [
  "~/.cargo/registry/src",   # source of every crate you depend on
  "~/go/pkg/mod",            # the Go module cache (or wherever GOMODCACHE points)
]
```

- Allow a toolchain's source cache, never the toolchain's home directory. `~/.cargo` holds
  `credentials.toml` (your crates.io token); `~/.cargo/registry/src` holds only source.
- The Rust standard library's source is under
  `~/.rustup/toolchains/<toolchain>/lib/rustlib/src`, one entry per toolchain.
  `allow_paths` does not expand globs.
- `~`, `$HOME`, or `/` as an allowed tree puts mail, keys, and documents in reach. kaibo
  allows it when named and logs a warning at startup. Agree on it with the person whose
  files they are before you set it.

The HTTP server refuses to start with no allowed tree. A service manager starts kaibo in
the home directory or `/`, and kaibo does not adopt a launch directory that holds the home
directory, so set `root`.

## The token

kaibo creates the token the first time the HTTP server starts:
`$XDG_STATE_HOME/kaibo/http-token`, else `~/.local/state/kaibo/http-token`. It holds 32
random bytes as base64url. It lives in the state directory, not the config directory, so a
dotfiles repo does not pick it up. Neither the log nor `kaibo://config` shows the token;
both show the path.

On Unix the file is owner-only (mode `0600`), and kaibo trusts it only while it is a
regular file, not a symlink, owned by kaibo's user with no group or other access. Anything
else is refused on every request, so a file someone else planted never becomes the token.
On Windows the file takes the permissions of its directory under your user profile.

| Command | Prints |
|---|---|
| `kaibo http-token` | the token file's path |
| `kaibo http-token --header` | `{"Authorization":"Bearer <token>"}`, for a header helper |
| `kaibo http-token --print` | the token |
| `kaibo http-token --rotate` | writes a new token, then prints the path |

The server reads the file on every request. After `--rotate`, the next request with the
old token gets `401`, and a client whose header helper runs `kaibo http-token --header`
re-reads the file and retries with no restart.

`kaibo http-token` reads `XDG_STATE_HOME` to find the file. A client that clears its
helper's environment (Codex does) finds `~/.local/state/kaibo/http-token`; if you moved
the state directory, put it in the command:
`XDG_STATE_HOME=/path kaibo http-token --header`.

## Connect a client

Each host passes the token its own way. Prefer a header helper: it follows rotation with
no restart. The client stanzas below were checked against each host's docs on 2026-11-02;
confirm against your version before copying.

### Claude Code

User scope, so every project sees it and the helper runs without a folder-trust prompt:

```json
// ~/.claude.json, or: claude mcp add --scope user --transport http kaibo http://127.0.0.1:52426/mcp
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

The helper runs at session start and on every reconnect, and again once after a `401`.
An HTTP request times out after 60 seconds unless `timeout` (or `MCP_TOOL_TIMEOUT`)
raises it; a `consult` often runs longer.

### Codex

```toml
# ~/.codex/config.toml
[mcp_servers.kaibo]
url = "http://127.0.0.1:52426/mcp"
http_headers_helper = "kaibo http-token --header"
tool_timeout_sec = 600
```

Codex runs the helper when it connects and again after a `401`, with a cleared
environment (see the `XDG_STATE_HOME` note above). It has no CLI flag for the helper; edit
`config.toml`. Do not also set `bearer_token_env_var`: an explicit bearer token wins over
the helper. Codex's command sandbox does not apply to MCP servers.

### OpenCode

```json
// ~/.config/opencode/opencode.json
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

OpenCode reads `{file:...}` when it loads its config, so after a rotation, touch
`opencode.json` or restart OpenCode. If you set `XDG_STATE_HOME`, write that path into the
`{file:...}` reference instead (`kaibo http-token` prints it). `oauth: false` keeps a `401` from starting an OAuth
flow. `codemode: false` lists kaibo's tools as native tools.

### Gemini CLI

```json
// ~/.gemini/settings.json, or: gemini mcp add -s user -t http kaibo http://127.0.0.1:52426/mcp
{
  "mcpServers": {
    "kaibo": {
      "url": "http://127.0.0.1:52426/mcp",
      "type": "http",
      "headers": { "Authorization": "Bearer $KAIBO_BEARER" }
    }
  }
}
```

Start Gemini CLI with the variable set: `KAIBO_BEARER=$(kaibo http-token --print) gemini`.
Gemini CLI has no header helper, so a rotation needs a restart. Keep `TOKEN`, `KEY`, and
`AUTH` out of the variable's name: Gemini CLI strips such variables. In a Docker or Podman
sandbox, `127.0.0.1` is the container, not your machine, so kaibo is unreachable; use the
stdio setup there.

### Claude Desktop

Use stdio. Desktop's custom connectors connect from Anthropic's cloud and cannot reach a
loopback address. Pass `--root` and an absolute command path, because Desktop's launch
directory is not documented.

## Run it as a service

### systemd (Linux)

```ini
# ~/.config/systemd/user/kaibo.service
[Unit]
Description=kaibo MCP server (HTTP)

[Service]
# --root: systemd starts a user service in your home directory, which kaibo does not
# adopt as scope. Drop it if config.toml sets [server] root.
ExecStart=%h/.cargo/bin/kaibo serve --http --root %h/src
Restart=on-failure
# Provider keys: api_key_file / api_key_cmd in config.toml need nothing here. For env
# keys, use an owner-only file:
# EnvironmentFile=%h/.config/kaibo/env

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now kaibo
journalctl --user -u kaibo -f        # the startup log names the URL and token file
loginctl enable-linger "$USER"       # optional: keep it running while logged out
```

### launchd (macOS)

launchd does not expand `~`; use absolute paths. launchd starts an agent in `/`, so pass
`--root` (or set `[server] root` in `config.toml`).

```xml
<!-- ~/Library/LaunchAgents/dev.kaibo.serve.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.kaibo.serve</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/bin/kaibo</string>
    <string>serve</string>
    <string>--http</string>
    <string>--root</string>
    <string>/Users/YOU/src</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/YOU/Library/Logs/kaibo.log</string>
</dict>
</plist>
```

```sh
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/dev.kaibo.serve.plist
launchctl kickstart -k "gui/$(id -u)/dev.kaibo.serve"   # restart after a config change
```

### Windows

Run kaibo inside WSL and use the systemd unit there (WSL needs `systemd=true` in
`/etc/wsl.conf`). WSL forwards `127.0.0.1` to Windows by default, so Windows clients
reach it at the same URL.

## What the server refuses

| Request | Response |
|---|---|
| carries an `Origin` header (a browser) | `403` |
| any path other than `/mcp`, including OAuth discovery under `/.well-known/` | `404` |
| no `Authorization: Bearer`, or not the current token | `401` with `WWW-Authenticate: Bearer` |
| a `Host` header that is not a loopback name or the bound address | `4xx`, even with the token |

Each refusal is logged with its reason, never with a header value. At most 64 connections
are served at once, a client gets 30 seconds to send its request headers, and on SIGINT or
SIGTERM the server lets open requests finish for up to 30 seconds before it exits.

## Preview limits

- One token, and every holder is the operator: all clients share sessions, jobs, and
  config. Per-client tokens are possible later.
- MCP log notifications are not sent over HTTP. Logs go to stderr (the journal under
  systemd); `job_wait` still reports job progress.
- Loopback only, plain HTTP. Use SSH port forwarding to reach another machine.
- After changing `config.toml`, restart the service: the server reads config once at
  startup. The token is the exception: it is re-read on every request.
