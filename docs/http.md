# kaibo over a Unix socket (preview)

kaibo serves MCP over stdio by default. This transport serves the same tools as streamable
HTTP from one long-lived process, on a Unix socket: a file, with no network address. Use it
when a host's sandbox makes launching a process hard, or to share one kaibo, with one
workspace and one set of provider keys, across several agents on one machine.

It is a preview, and an early one: it works with any client that can open a Unix socket
(rmcp's `UnixSocketHttpClient` does), and the agent hosts are still catching up, so
`kaibo://hosts/<host>` has no config entry for one yet. The flags keep the name `--http`
because the protocol is HTTP; the transport is the socket.

The door is the file's mode. The socket is `0600`, so only the user who started kaibo can
open it, and the server also drops any connection whose uid is not its own. There is no
token to store, rotate, or leak, and a browser cannot open a Unix socket. Every client that
connects is the operator, so sessions, jobs, and `kaibo://config` are shared by all of them.

## Turn it on

```toml
# ~/.config/kaibo/config.toml
[server]
root = "~/src"

[server.http]
enabled = true
# socket = "/run/user/1000/kaibo/mcp.sock"   # the default: $XDG_RUNTIME_DIR/kaibo/mcp.sock
```

Then run `kaibo serve`. The same switches exist as `kaibo serve --http --socket PATH` and
`KAIBO_HTTP=1` / `KAIBO_HTTP_SOCKET`. `kaibo serve --no-http` or `KAIBO_HTTP=0` serves
stdio for one run without editing the file. The startup log prints the socket path, and
`kaibo://config` shows it under `[http]`.

Where the socket goes:

- `$XDG_RUNTIME_DIR/kaibo/mcp.sock` where the system sets that directory (Linux with
  systemd). It is per user, mode `0700`, and emptied at logout.
- `$XDG_STATE_HOME/kaibo/mcp.sock` (`~/.local/state/kaibo/mcp.sock`) where it is unset,
  as on macOS.
- `socket` must be an absolute path of at most 107 bytes (103 on macOS), and outside every
  allowed tree: a path inside `--root` is refused.

The directory the socket lives in, and each parent of it, must belong to you or root and
be closed to other users (a sticky directory such as `/tmp` is fine); otherwise another
user could swap the socket for their own, and the server refuses to start.

The server creates the socket's directory, replaces a stale socket a crash left behind, and
removes its socket on SIGINT or SIGTERM. It never removes a file that is not a socket owned
by its user, and it refuses to start when another server answers on the path.

To reach kaibo from another machine, forward the socket over SSH:
`ssh -L /tmp/kaibo.sock:/run/user/1000/kaibo/mcp.sock host` (OpenSSH 6.7 or later).

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

Scoping to `~`, `$HOME`, or `/` also covers the default socket path where there is no
`XDG_RUNTIME_DIR` (it is then under `$HOME`), and the server refuses it: set `socket` to a
path outside the tree.

The socket server refuses to start with no allowed tree. A service manager starts kaibo in
the home directory or `/`, and kaibo does not adopt a launch directory that holds the home
directory, so set `root`.

## Connect a client

A client needs two things: the socket path, and HTTP at `/mcp` over it. In Rust, rmcp's
client does both:

```rust
let uri = "http://localhost/mcp";
let client = UnixSocketHttpClient::new("/run/user/1000/kaibo/mcp.sock", uri);
let transport = StreamableHttpClientTransport::with_client(
    client,
    StreamableHttpClientTransportConfig::with_uri(uri),
);
```

(rmcp 3.11, feature `transport-streamable-http-client-unix-socket`.) From a shell,
`curl --unix-socket "$XDG_RUNTIME_DIR/kaibo/mcp.sock" http://localhost/mcp` reaches the
same endpoint. The agent hosts in `kaibo://hosts` use stdio until they can dial a socket.

## Run it as a service

### systemd (Linux)

```ini
# ~/.config/systemd/user/kaibo.service
[Unit]
Description=kaibo MCP server (Unix socket)

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
journalctl --user -u kaibo -f        # the startup log names the socket path
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

Windows has no Unix socket server here: `kaibo serve --http` refuses to start. Run kaibo
inside WSL and use the systemd unit there (WSL needs `systemd=true` in `/etc/wsl.conf`).

## What the server refuses

| Request | Response |
|---|---|
| a connection from another uid | dropped before a byte is read |
| any path other than `/mcp`, including OAuth discovery under `/.well-known/` | `404` |
| a socket path inside an allowed tree, relative, or too long | the server does not start |
| a regular file, or another user's socket, at the path | the server does not start, and the file stays |

Each refusal is logged with its reason. At most 64 connections are served at once, a client
gets 30 seconds to send its request headers, and on SIGINT or SIGTERM the server lets open
requests finish for up to 30 seconds before it exits.

## Preview limits

- Every client that can open the socket is the operator: all share sessions, jobs, and
  config.
- MCP log notifications are not sent over this transport. Logs go to stderr (the journal
  under systemd); `job_wait` still reports job progress.
- Unix only. There is no TCP or TLS listener, by design.
- After changing `config.toml`, restart the service: the server reads config once at
  startup.
