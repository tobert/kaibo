//! Per-host setup guides, served as `kaibo://hosts/{name}` and printed by
//! `kaibo host-guide <name>`. Each guide is a markdown file under `docs/hosts/`, embedded
//! so `cargo install kaibo` carries it. One file per host is the single source: the HTTP
//! guide and the configuration guide link here instead of repeating client stanzas.

/// One host's guide.
pub(crate) struct HostGuide {
    /// The name in `kaibo://hosts/{name}` and `kaibo host-guide <name>`.
    pub name: &'static str,
    /// What the index says about it.
    pub summary: &'static str,
    pub body: &'static str,
}

/// Every host kaibo documents, in index order.
pub(crate) const HOST_GUIDES: &[HostGuide] = &[
    HostGuide {
        name: "claude-code",
        summary: "Claude Code: user-scope stdio, or HTTP with `headersHelper`",
        body: include_str!("../../docs/hosts/claude-code.md"),
    },
    HostGuide {
        name: "codex",
        summary: "Codex CLI: cleared server environment, 60-second tool timeout, no prompts",
        body: include_str!("../../docs/hosts/codex.md"),
    },
    HostGuide {
        name: "opencode",
        summary: "OpenCode: V2 config, turn Code Mode off, `{file:...}` token",
        body: include_str!("../../docs/hosts/opencode.md"),
    },
    HostGuide {
        name: "agy",
        summary: "Antigravity CLI (agy), which replaces Gemini CLI: `agy mcp add`",
        body: include_str!("../../docs/hosts/agy.md"),
    },
    HostGuide {
        name: "claude-desktop",
        summary: "Claude Desktop: stdio only, absolute paths, `--root` required",
        body: include_str!("../../docs/hosts/claude-desktop.md"),
    },
];

/// The guide for `name`, if kaibo has one.
pub(crate) fn host_guide(name: &str) -> Option<&'static str> {
    HOST_GUIDES.iter().find(|g| g.name == name).map(|g| g.body)
}

/// Every guide's name, comma-separated, for refusals.
pub(crate) fn host_names() -> String {
    HOST_GUIDES
        .iter()
        .map(|g| g.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `kaibo://hosts` index. `link` renders how to open one guide, so the MCP and CLI
/// front doors each name the form their reader can use.
pub(crate) fn host_index(link: impl Fn(&str) -> String) -> String {
    let mut out = String::from(
        "# Host setup guides\n\n\
         Setup for the agent host that launches kaibo or connects to it: the config \
         stanza, how it starts kaibo, its timeouts, approvals, and sandbox, and which MCP \
         features it supports. Each guide says what version it was checked against.\n\n",
    );
    for g in HOST_GUIDES {
        out.push_str(&format!("- {}: {}\n", link(g.name), g.summary));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_guide_is_reachable_and_dated() {
        for g in HOST_GUIDES {
            assert_eq!(host_guide(g.name), Some(g.body));
            assert!(
                g.body.contains("Checked against"),
                "{}: a host guide names what it was checked against",
                g.name
            );
            assert!(
                g.body.contains("kaibo://configure"),
                "{}: a host guide names the configure resource",
                g.name
            );
        }
        assert_eq!(host_guide("gemini-cli"), None, "Gemini CLI is retired");
        let index = host_index(|n| format!("`kaibo://hosts/{n}`"));
        for g in HOST_GUIDES {
            assert!(index.contains(&format!("`kaibo://hosts/{}`", g.name)));
        }
    }
}
