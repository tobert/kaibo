# kaibo with Codex

Use kaibo for reviews and second opinions from another model family. Hosted casts
send your question, supplied context, and the source they read to their configured
providers. Read-only protects the project from changes; it does not keep its content
on your machine. Choose provider access during setup. Keep approval for paid calls separate from
network access; Codex and the user decide whether an approval applies once or persists.

## Start with MCP

In Codex's `config.toml`, use an absolute executable path and one project root:

```toml
[mcp_servers.kaibo]
command = "/absolute/path/to/kaibo"
args = ["--root", "/absolute/path/to/project"]
```

Restart the MCP server after changing its launch configuration. Read `kaibo://config`
to check `allowed_paths`, the default cast, key sources, and persistence. Linked
worktrees of an allowed repository are followed by default. Add another project with
`--allow-path /absolute/path/to/other-project` when it is in the agreed scope.

Codex launches a local stdio MCP server separately from its sandboxed shell commands.
Command network rules do not restrict MCP servers. A wrapper, remote executor, or
outer container can still restrict the server. Probe the actual MCP connection before
changing shell permissions. Codex's [MCP documentation](https://learn.chatgpt.com/docs/extend/mcp)
and [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
describe the host controls.

For a key from the environment, declare `api_key_env` in kaibo's backend and forward
that variable with Codex's `mcp_servers.kaibo.env_vars`. A declared `api_key_file` can
avoid environment forwarding. Keep values out of chat and configuration examples.
Config loads at server startup; keys resolve when kaibo builds each model client.

Prefer `consult_submit` followed by `job_get` or `job_wait` for long reviews. Codex's
MCP tool timeout defaults to 60 seconds; `tool_timeout_sec` changes it. A client timeout
does not establish whether the provider stopped working or billing.

## Consent and MCP messages

Codex owns tool approval. To request approval for model-driven calls while allowing
model listing, set a server default and a narrow exception:

```toml
[mcp_servers.kaibo]
command = "/absolute/path/to/kaibo"
args = ["--root", "/absolute/path/to/project"]
default_tools_approval_mode = "prompt"

[mcp_servers.kaibo.tools.list_models]
approval_mode = "approve"
```

This covers `consult`, `consult_submit`, `explore`, `oneshot`, `deliberate`,
`batch_submit`, and `generate`, including future tools unless explicitly overridden.
Review existing per-tool overrides: they win over the server default. Resources are
read through MCP's resource interface, separately from tool-call approval.

`auto` uses Codex's policy, `prompt` requests approval, `writes` requests it for tools
not marked read-only, and `approve` allows the tool without that prompt. Read-only
calls can still spend API credits and send source, so `writes` does not express paid
call consent. The top-level `approvals_reviewer` selects `user` or `auto_review` for
approval requests; `prompt` is not a guarantee of a human dialog in auto-review mode.
Auto-review sees the approval request and retained conversation, so state the user's
scope and consent explicitly. After a denial, explain it; Codex's `/approve` command
can authorize one exact retry, which still goes through review. See
[Codex auto-review](https://learn.chatgpt.com/docs/sandboxing/auto-review).

Honor a user's explicit persistent approval through the host's controls. kaibo does
not store or infer “don't ask again,” and a new session should use Codex's actual
saved policy. A provider-domain allow rule does not express consent to a model call. Conversely,
a tool approval does not configure network or filesystem access.

MCP provides server instructions, resources, prompts, tool results, and
`elicitation/create` for requesting user input. Codex supports form and URL
elicitation. A server must check the client's advertised capability before sending
one; kaibo does not currently send elicitation requests. Form elicitation is unsuitable
for secrets. See the [MCP elicitation specification](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation).

Elicitation acceptance is a response to the server, not an OS permission grant.
Codex's command and filesystem permission requests are separate app-server messages,
owned by Codex. We found no documented MCP request that grants a stdio server broader
host permissions. See [Codex app-server approvals](https://learn.chatgpt.com/docs/app-server#approvals).
Use kaibo's guidance to explain the needed access; let Codex apply its approval policy.

## CLI access

A `kaibo` command invoked by Codex's shell inherits that shell's sandbox. Test it
separately from MCP. A connection error can come from networking, DNS, a proxy, TLS,
or an unavailable endpoint; it does not prove the provider rejected the request.

When the user has authorized the consultation, request the missing access through
Codex's available permission mechanism. Name the provider, the project/context being
sent, and any state directories needed. If automatic review refuses it, report the
refusal and ask the user for the specific consent still needed. A failed CLI call is
not permission to switch to MCP or another provider outside the authorized scope.

MCP tool approval settings do not cover CLI commands. If paid calls must always go
through tool approval, use MCP. A CLI workflow needs its own Codex command rules or
approval instructions; enabling network access alone will not prompt for API spend.

For recurring CLI work, a named Codex permissions profile can extend `:workspace` and
allow just the configured provider hosts through its network proxy:

```toml
[features]
network_proxy = true

[permissions.kaibo-review]
extends = ":workspace"

[permissions.kaibo-review.network]
enabled = true

[permissions.kaibo-review.network.domains]
"api.deepseek.com" = "allow"
```

Select it with top-level `default_permissions = "kaibo-review"` only when that should
be the session default. This policy covers all sandboxed commands, not just kaibo.
Domain rules need the proxy enabled. Adapt an existing profile rather than replacing
its restrictions. Managed policy can limit available profiles. Older setups use
`sandbox_workspace_write.network_access`, which enables broader command networking.
See [Codex permissions profiles](https://learn.chatgpt.com/docs/permissions).

Persistent sessions need writes to the state directory, including database sidecars;
disk artifacts need writes to the CAS directory. Grant only the chosen directories,
using the profile's `filesystem` rules or legacy `sandbox_workspace_write.writable_roots`.
The defaults are `~/.local/state/kaibo` and `~/.local/share/kaibo/cas` when XDG variables
are unset. Read the resolved paths first. State and CAS must stay outside kaibo's
allowed project trees. `--state-db` and `--cas-dir` support separate per-client stores.

`--no-persistence` deliberately gives up durable sessions and makes the CAS in-memory;
it is not a repair for a blocked durable store. Include only enabled optional endpoints
in the network policy: local models, key-command services, telemetry collectors, and
media download hosts can need access beyond the completion provider.

## Diagnose one boundary at a time

| Observation | Next check |
|---|---|
| kaibo says a path is outside the allowed set | Check `kaibo://config`; correct the root or add the authorized tree, then restart MCP. |
| MCP works; CLI cannot connect | Check the command sandbox and its network policy. |
| A key is missing | Check the declared source and MCP `env_vars`; key values belong in the operator's environment, file, or vault. |
| State cannot open | Check the exact state directory and its write permission; preserve the existing database. |
| A model returns an authentication or request error | Check that backend's credentials or model settings. |
| A synchronous MCP call times out | Use the async tools or adjust Codex's tool timeout. |

This guide is embedded as `kaibo://config/codex` and `kaibo config-guide codex`.
Check `codex --version` and its installed help when adapting examples; the host settings
above were checked against Codex CLI 0.154.0 and official docs on 2026-09-13.
