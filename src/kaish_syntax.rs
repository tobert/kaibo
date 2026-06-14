//! One source of truth for how kaibo describes its read-only kaish shell.
//!
//! kaish now single-sources its own guidance in the `kaish-help` crate (reached
//! here via `kaish_kernel::help`), so the generic "how kaish works" contract no
//! longer lives — and drifts — in kaibo. We *compose*: [`kaish_operating_contract`]
//! is the canonical kaish foundations straight from `kaish-help`, and
//! [`KAISH_SANDBOX_ADDENDUM`] layers on the facts that are kaibo's alone — the
//! read-only boundary, the exit-code contract a caller will misread without help,
//! the no-persistent-cwd rule, and where to learn more. [`kaish_syntax_core`] is
//! the two stitched together: the compact, model-facing block every preamble and
//! the internal `run_kaish` tool definition embed.
//!
//! The topic and per-builtin renderers ([`render_topic`], [`render_builtin_help`])
//! and [`kaibo_sandbox_doc`] back the `kaibo://kaish/*` resources, so an agent can
//! progressively learn more kaish — syntax, scatter, the builtin index, a single
//! builtin's parameters — without spending a tool turn.

use std::path::PathBuf;
use std::sync::OnceLock;

use kaish_kernel::help::{
    compose, get_help, list_topics, tool_help, HelpTopic, Recipe, SchemaContent,
};
use kaish_kernel::tools::ToolSchema;

use crate::config::{CastUsability, Config};

/// The kaibo-specific half of the core: the read-only boundary, the exit-code
/// contract, the no-cwd rule, and the line-number idioms that make citations
/// exact. These are *not* in `kaish-help` — they describe kaibo's sandbox, not
/// kaish the language — so they're authored here and layered onto the canonical
/// contract. Positive framing on purpose (weaker/local models loop on blanket
/// prohibitions): "just read", not a wall of "never".
pub const KAISH_SANDBOX_ADDENDUM: &str = "\
In kaibo this shell runs over a READ-ONLY snapshot of one project, offline: writes, \
`git`, `touch`, and external commands are refused, so just read. Browse with line \
numbers so every citation is exact — `cat -n FILE`, `rg -n PATTERN`, and numbered \
spans like `cat -n FILE | sed -n '40,80p'`. Each call starts at the project root; \
there is no persistent cwd. Read the exit code: 0 is success; 3 means the output \
was too large and came back as a head+tail sample (not a failure); 124 means the \
script was killed for running past its time budget; 126 means blocked by the \
read-only sandbox; 127 is command-not-found; any other non-zero means the script \
itself failed. Want to go deeper? Run `help`, `help syntax`, or `help <builtin>` in \
any script, or read the `kaibo://kaish/*` resources.";

/// The canonical kaish operating contract, sourced from `kaish-help` so kaibo
/// never re-states (and drifts from) kaish's own guidance. This is exactly what
/// kaish-mcp puts on its `execute` tool: the foundations (no word splitting,
/// structured output, …) as terse rules and bash contrasts. Composed once.
pub fn kaish_operating_contract() -> &'static str {
    static CONTRACT: OnceLock<String> = OnceLock::new();
    CONTRACT.get_or_init(|| compose(&Recipe::tool_description(), &SchemaContent::new(&[])))
}

/// The compact, model-facing cheatsheet: the canonical kaish contract plus kaibo's
/// sandbox addendum. Every internal preamble and the internal `run_kaish` tool
/// definition embed this, so there is exactly one place the model-facing framing
/// lives. Composed once.
pub fn kaish_syntax_core() -> &'static str {
    static CORE: OnceLock<String> = OnceLock::new();
    CORE.get_or_init(|| {
        format!(
            "{}\n\n{}",
            kaish_operating_contract(),
            KAISH_SANDBOX_ADDENDUM
        )
    })
}

/// The internal `run_kaish` (rig) tool description shown to kaibo's own models. It
/// *is* the shared core — same contract, same sandbox facts, no drift.
pub(crate) fn run_kaish_tool_description() -> String {
    kaish_syntax_core().to_string()
}

/// Render a kaish help topic (`syntax`, `builtins`, `scatter`, …) to markdown,
/// straight from `kaish-help`. `Builtins` and `Tool(_)` topics need the live
/// `schemas`; the static topics ignore them. Backs the `kaibo://kaish/{topic}`
/// resources.
pub fn render_topic(topic: &str, schemas: &[ToolSchema]) -> String {
    get_help(&HelpTopic::parse_topic(topic), schemas)
}

/// Render help for a single builtin, or `None` if no such builtin is registered.
/// Backs the `kaibo://kaish/builtin/{name}` resource template — `None` becomes a
/// not-found, not a misleading "unknown topic" body.
pub fn render_builtin_help(name: &str, schemas: &[ToolSchema]) -> Option<String> {
    tool_help(name, schemas)
}

/// The kaish help topics kaibo surfaces as resources, as `(name, description)`.
/// This is `kaish-help`'s own registry verbatim, so kaibo's resource list tracks
/// upstream topics automatically — add a topic there, it shows up here.
pub fn topics() -> Vec<(&'static str, &'static str)> {
    list_topics()
}

/// kaibo's MCP `instructions:` — what a host hands a model before its first call.
///
/// Composes kaish-help's `agent_onboarding` recipe (the mental model, the operating
/// contract, and the live builtin index from `schemas`) so the onboarding tracks
/// kaish upstream, then frames it as kaibo: read-only, four tools, and where to
/// learn more. The canonical block carries the "how kaish works" spine; we add only
/// what's kaibo's.
pub fn kaibo_instructions(schemas: &[ToolSchema]) -> String {
    let onboarding = compose(&Recipe::agent_onboarding(), &SchemaContent::new(schemas));
    format!(
        "kaibo (解剖) — ask a question about a codebase and get a grounded, cited \
         answer. kaibo reads the project READ-ONLY through a kaish shell and never \
         modifies files or runs external commands. Tools: `consult` (capable model, \
         reads spans and delegates broad sweeps), `explore` (fast curated report), \
         `synthesize` (capable model over optional context), and `run_kaish` (drive \
         the shell directly). Each is gated independently, so a given server may \
         advertise only some.\n\n\
         The shell is kaish. Here is how it works:\n\n\
         {onboarding}\n\n\
         ## Learn more kaish\n\
         Read the `kaibo://kaish/*` resources to go deeper without spending a tool \
         turn — `kaibo://kaish/syntax`, `kaibo://kaish/builtins`, `kaibo://kaish/vfs`, \
         `kaibo://kaish/scatter`, and the rest — or `kaibo://kaish/builtin/<name>` for \
         one builtin. `kaibo://kaish/sandbox` states kaibo's read-only contract and \
         exit codes. It's all in the shell too: `help`, `help syntax`, `help <builtin>`."
    )
}

/// The setup-guidance block prepended to the instructions when the default cast has
/// no usable provider (a fresh `cargo install` with no key set). Steers toward an env
/// var or a key file — *never* pasting the key into the chat — names the default cast's
/// backends and their key sources, points at the example resource, and reminds the user
/// to reconnect the server (which only re-reads the environment and config at startup).
///
/// Positive framing: it leads with what *works now* (`run_kaish` needs no provider) and
/// what to do, not a wall of "you can't". Best-effort on the backend list — if the
/// default cast doesn't resolve we still emit the general steps.
fn setup_section(config: &Config) -> String {
    let mut lines = Vec::new();
    if let Ok(cast) = config.resolve_cast(&config.default_cast) {
        let mut seen = std::collections::BTreeSet::new();
        for slot in cast.slots.values() {
            if let Ok(b) = config.resolve_backend(&slot.backend) {
                if seen.insert(b.name.clone()) {
                    let env = b.api_key_env.as_deref().unwrap_or("(none)");
                    let file = b.api_key_file.as_deref().unwrap_or("(none)");
                    lines.push(format!(
                        "  - backend `{}` ({}) — set env `{}`, or write the key to `{}`",
                        b.name,
                        b.kind.canonical_name(),
                        env,
                        file
                    ));
                }
            }
        }
    }
    let backends = if lines.is_empty() {
        "  - the default cast names no backends yet — set `cast` in config.toml".to_string()
    } else {
        lines.join("\n")
    };

    format!(
        "## Setup needed — no model provider configured\n\
         kaibo's default cast `{cast}` has no usable API key, so `consult`, `explore`, \
         and `synthesize` can't reach a model yet. `run_kaish` works right now — it drives \
         the read-only shell with no model in the loop — so you can browse the code while \
         you set a key.\n\n\
         Give the default cast a key. Prefer an **environment variable** or a **key file** \
         — kaibo reads either at startup, so the key stays out of this conversation:\n\
         {backends}\n\n\
         Keep the API key out of the chat: set the env var or key file yourself in your \
         shell, don't paste it to the model (if you do, that's your call — kaibo doesn't \
         need to see it). Then **reconnect the kaibo MCP server** so it re-reads the \
         environment and config — they're read once at startup. In Claude Code: run `/mcp` \
         and reconnect kaibo; other hosts have their own reload.\n\n\
         Prefer a different provider? Pass `provider=\"<name>\"` on a call (a backend or \
         cast — e.g. a local keyless endpoint), or set `cast` in config. The full annotated \
         example is the `kaibo://config/example` resource; it belongs at \
         `~/.config/kaibo/config.toml` (or `$XDG_CONFIG_HOME/kaibo/config.toml`). Read \
         `kaibo://config` for the current resolved state.",
        cast = config.default_cast,
    )
}

/// The `## Casts` block: the casts that can reach a model *right now* (from
/// [`Config::usable_casts`]), the default marked and a local/unverified one tagged.
/// This is the handshake answering "what can I pass as `cast`?" truthfully — it
/// names config.toml casts the static per-tool `cast` enum can't, and lists only
/// what will actually work (an unconfigured cast is filtered upstream). It closes by
/// pointing at `kaibo://config` as canonical for the full configured state, since
/// this list is deliberately partial (usable-only) and read once at startup.
///
/// Empty `usable` (no cast can reach a model) renders nothing — the `Unconfigured`
/// setup banner already owns that case and would otherwise say it twice. Returns a
/// trailing `\n\n` so the caller can splice it in unconditionally.
fn casts_section(default_cast: &str, usable: &[(String, CastUsability)]) -> String {
    if usable.is_empty() {
        return String::new();
    }
    let lines: String = usable
        .iter()
        .map(|(name, state)| {
            let mut tags = Vec::new();
            if name == default_cast {
                tags.push("default");
            }
            if matches!(state, CastUsability::LocalUnverified) {
                tags.push("local, unverified");
            }
            if tags.is_empty() {
                format!("- `{name}`")
            } else {
                format!("- `{name}` ({})", tags.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## Casts\n\
         A cast is the model team that staffs a consultation; pass `cast=<name>` on a \
         call to pick one. Usable right now (resolved at startup — reconnect after a \
         config or key change):\n\
         {lines}\n\n\
         `kaibo://config` is canonical for what's configured — every cast and backend \
         (with aliases), the gated tools, and sandbox limits.\n\n"
    )
}

/// Like [`kaibo_instructions`] but with a **scope section** appended so the calling
/// model always knows:
/// - the default root (or that every call must pass one),
/// - the allowed trees a per-call `path` must be at-or-under, and
/// - that `kaibo://config` has the full picture.
///
/// When `usability` is [`CastUsability::Unconfigured`] (a fresh install with no key),
/// a [`setup_section`] is prepended so the calling model can walk the user through
/// configuration. `Ready`/`LocalUnverified` get the normal instructions unchanged.
///
/// Used by `get_info` so every `initialize` handshake surfaces the server's
/// containment posture. Unit-testable: pass your own `Config`, `allowed_set`, and
/// `usability` rather than fabricating a `RequestContext` or reading the environment.
pub fn kaibo_instructions_with_scope(
    schemas: &[ToolSchema],
    config: &Config,
    allowed_set: &[PathBuf],
    usability: CastUsability,
    usable_casts: &[(String, CastUsability)],
) -> String {
    // The unconfigured-install banner leads, so a fresh user sees it first.
    let setup = match usability {
        CastUsability::Unconfigured => format!("{}\n\n", setup_section(config)),
        CastUsability::Ready | CastUsability::LocalUnverified => String::new(),
    };
    let base = kaibo_instructions(schemas);
    let casts = casts_section(&config.default_cast, usable_casts);

    // Scope section: always accurate, never ambiguous.
    let root_line = match &config.root {
        Some(r) => format!("- **Default root:** `{}`", r.display()),
        None => "- **Default root:** none — every call must pass a `path` argument.".to_string(),
    };
    let allowed_lines: String = allowed_set
        .iter()
        .map(|p| format!("  - `{}`", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{setup}{base}\n\n\
         {casts}\
         ## Scope\n\
         This server's path containment is always on. A per-call `path` must \
         canonicalize to at-or-under one of the allowed trees below.\n\n\
         {root_line}\n\
         - **Allowed trees:**\n\
         {allowed_lines}\n\n\
         Read `kaibo://config` for the full resolved runtime configuration — \
         default cast, gated tools, sandbox limits, and every backend and cast \
         (with their aliases)."
    )
}

/// The kaibo-authored read-only boundary doc, served at `kaibo://kaish/sandbox`.
/// Where the canonical topics describe kaish, this describes *kaibo's* sandbox:
/// the read-only contract and the exit codes an automated caller must classify.
/// A verbose superset of [`KAISH_SANDBOX_ADDENDUM`].
pub fn kaibo_sandbox_doc() -> String {
    format!(
        "# kaibo — the read-only kaish sandbox\n\n\
         {KAISH_SANDBOX_ADDENDUM}\n\n\
         ## Browsing for exact citations\n\
         Lead with line numbers so every claim cites `file:line`:\n\
         - `cat -n FILE` — file with line numbers\n\
         - `rg -n PATTERN [PATH]` — matches with line numbers\n\
         - `cat -n FILE | sed -n '40,80p'` — a numbered span\n\
         - `rg -l PATTERN src` — just the file names that match\n\n\
         ## Read-only boundary\n\
         The project is mounted read-only and external commands are off, by \
         construction. Writes, `git`, `touch`, `spawn`/`exec`, and any external \
         command are refused. This is the product: read freely, expect no write to \
         land.\n\n\
         ## Exit codes\n\
         - `0` — success\n\
         - `3` — output exceeded the cap and was truncated to a head+tail sample \
         (not a failure; the full output is not returned)\n\
         - `124` — killed for exceeding the per-exec time budget\n\
         - `126` — blocked by the read-only sandbox (collides with POSIX \
         \"not executable\" — read the message to be sure)\n\
         - `127` — command not found\n\
         - other non-zero — the script itself failed\n\n\
         ## Learn more kaish\n\
         These `kaibo://kaish/*` resources mirror kaish's own help, so you can go \
         deeper without spending a tool turn: `kaibo://kaish/syntax`, \
         `kaibo://kaish/builtins`, `kaibo://kaish/vfs`, `kaibo://kaish/scatter`, and \
         the rest. For one builtin, read `kaibo://kaish/builtin/<name>` (e.g. \
         `kaibo://kaish/builtin/rg`). All of it is also available inside a script: \
         `help`, `help syntax`, `help <builtin>`.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_layers_the_canonical_contract_under_the_kaibo_addendum() {
        let core = kaish_syntax_core();
        // The canonical half is sourced from kaish-help, not hand-rolled here — so
        // it must appear verbatim. This fails the moment the compose recipe breaks
        // or the layering drops it.
        let contract = kaish_operating_contract();
        assert!(
            !contract.is_empty(),
            "the canonical contract must compose to something"
        );
        assert!(
            core.contains(contract),
            "core must embed the canonical kaish contract verbatim"
        );
        // The kaibo half must be there too.
        assert!(
            core.contains(KAISH_SANDBOX_ADDENDUM),
            "core must embed the kaibo sandbox addendum verbatim"
        );
    }

    #[test]
    fn canonical_contract_carries_a_load_bearing_kaish_guarantee() {
        // kaish-help's Foundations lead with no-word-splitting; if the recipe ever
        // stops yielding it, our onboarding silently loses its spine — catch that.
        assert!(
            kaish_operating_contract().to_lowercase().contains("word"),
            "expected the no-word-splitting guarantee from kaish-help, got:\n{}",
            kaish_operating_contract()
        );
    }

    #[test]
    fn the_tool_description_is_the_core() {
        assert_eq!(run_kaish_tool_description(), kaish_syntax_core());
    }

    #[test]
    fn addendum_states_the_exit_code_contract_and_line_browsing() {
        // The two things the synth preamble rewards (exact file:line) and the two
        // codes an automated caller will misread without help. These are kaibo's,
        // so they live in the addendum (kaish-help can't know our 126/124).
        for needle in ["cat -n", "rg -n", "126", "124"] {
            assert!(
                KAISH_SANDBOX_ADDENDUM.contains(needle),
                "addendum must mention {needle:?}"
            );
        }
    }

    #[test]
    fn sandbox_doc_is_a_superset_of_the_addendum() {
        assert!(
            kaibo_sandbox_doc().contains(KAISH_SANDBOX_ADDENDUM),
            "the verbose sandbox doc must embed the addendum verbatim"
        );
    }

    #[test]
    fn topics_match_kaish_help_and_render_nonempty() {
        let topics = topics();
        assert!(
            topics.iter().any(|(t, _)| *t == "syntax"),
            "expected the syntax topic, got {topics:?}"
        );
        // A static topic renders without schemas.
        let syntax = render_topic("syntax", &[]);
        assert!(
            syntax.contains("Variables"),
            "syntax topic should cover Variables:\n{syntax}"
        );
    }

    /// A fresh install (Unconfigured) gets the setup banner: it leads, names the
    /// default cast's key sources, steers the key out of the chat, points at the
    /// example resource, and tells the user to reconnect the server.
    #[test]
    fn instructions_lead_with_setup_when_unconfigured() {
        let config = Config::builtin(); // default cast "anthropic"
        let text = kaibo_instructions_with_scope(
            &[],
            &config,
            &[PathBuf::from("/tmp")],
            CastUsability::Unconfigured,
            &[],
        );
        assert!(
            text.contains("Setup needed"),
            "must flag the setup state:\n{text}"
        );
        // The banner leads — a fresh user sees it before the rest.
        assert!(
            text.trim_start().starts_with("## Setup needed"),
            "setup banner must come first:\n{text}"
        );
        // Names the default cast's backend key sources (env + file), steers privacy,
        // keeps run_kaish usable, points at the example, and asks for a reconnect.
        for needle in [
            "ANTHROPIC_API_KEY",
            "key file",
            "run_kaish",
            "kaibo://config/example",
            "/mcp",
        ] {
            assert!(
                text.contains(needle),
                "setup banner must mention {needle:?}:\n{text}"
            );
        }
        assert!(
            text.contains("out of the chat") || text.contains("don't paste"),
            "setup banner must steer the key out of the conversation:\n{text}"
        );
    }

    /// A configured install (Ready) — and an unprobed-local one (LocalUnverified) —
    /// get the normal instructions, no setup banner nagging them.
    #[test]
    fn instructions_omit_setup_when_usable() {
        let config = Config::builtin();
        for usability in [CastUsability::Ready, CastUsability::LocalUnverified] {
            let text = kaibo_instructions_with_scope(
                &[],
                &config,
                &[PathBuf::from("/tmp")],
                usability,
                &[],
            );
            assert!(
                !text.contains("Setup needed"),
                "{usability:?} must not get the setup banner:\n{text}"
            );
        }
    }

    /// The handshake lists the casts that can reach a model *right now* — the
    /// truthful, startup-resolved answer to "what can I pass as `cast`?", including
    /// config.toml casts the static tool-schema enum can't name. The default is
    /// marked; a local/unverified one is tagged; an unconfigured cast is *not*
    /// advertised as usable; and the section points at `kaibo://config` as canonical
    /// for the full configured state.
    #[test]
    fn instructions_list_usable_casts_and_point_at_config() {
        let config = Config::builtin(); // default cast "anthropic"
        let usable = vec![
            ("anthropic".to_string(), CastUsability::Ready),
            ("mylocal".to_string(), CastUsability::LocalUnverified),
        ];
        let text = kaibo_instructions_with_scope(
            &[],
            &config,
            &[PathBuf::from("/tmp")],
            CastUsability::Ready,
            &usable,
        );
        assert!(
            text.contains("## Casts"),
            "must have a Casts section:\n{text}"
        );
        // Both usable casts are named, including the config.toml one.
        for needle in ["anthropic", "mylocal"] {
            assert!(
                text.contains(needle),
                "Casts section must name usable cast {needle:?}:\n{text}"
            );
        }
        // The default is marked.
        assert!(
            text.contains("(default)"),
            "Casts section must mark the default cast:\n{text}"
        );
        // A built-in cast absent from the usable list (no key) is NOT advertised —
        // gemini only appears if something names it, and nothing here does.
        assert!(
            !text.contains("gemini"),
            "an unconfigured cast must not be advertised as usable:\n{text}"
        );
        // Points at the config resource as canonical for what's configured.
        assert!(
            text.contains("canonical") && text.contains("kaibo://config"),
            "Casts section must point at kaibo://config as canonical:\n{text}"
        );
    }

    #[test]
    fn builtin_help_resolves_a_known_tool_and_rejects_an_unknown_one() {
        let schemas = vec![ToolSchema::new("cat", "Read a file")];
        let cat = render_builtin_help("cat", &schemas).expect("cat is registered");
        assert!(
            cat.contains("cat"),
            "builtin help should name the tool:\n{cat}"
        );
        assert!(
            render_builtin_help("definitely-not-a-builtin", &schemas).is_none(),
            "an unregistered builtin must render to None, not a stub"
        );
    }
}
