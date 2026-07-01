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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use kaish_kernel::help::{
    compose, get_help, list_topics, tool_help, HelpTopic, Recipe, SchemaContent,
};
use kaish_kernel::tools::ToolSchema;

use crate::config::{CastUsability, Config, ModelRole};

/// The kaibo-specific half of the core: the read-only boundary, the exit-code
/// contract, the no-cwd rule, and the line-number idioms that make citations
/// exact. These are *not* in `kaish-help` — they describe kaibo's sandbox, not
/// kaish the language — so they're authored here and layered onto the canonical
/// contract. Positive framing on purpose (weaker/local models loop on blanket
/// prohibitions): "just read", not a wall of "never".
pub const KAISH_SANDBOX_ADDENDUM: &str = "\
In kaibo this shell runs over a READ-ONLY snapshot of one project, offline: writes, \
`git`, `touch`, and external commands are refused, so just read. Browse with line \
numbers so every citation is exact, and read generously — the context window is \
yours to fill, so favor one wide look over many narrow ones; reading a whole file \
often surfaces what a surgical slice would hide. `cat -n FILE` reads a file whole \
with its line numbers — reach for it first; most files are short (`wc -l FILE` \
confirms). To locate something across files, `grep -rn -B3 -A6 PATTERN .` returns \
each match with the lines around it. A whole-file read that comes back truncated \
(exit 3, a head+tail sample) was simply too big — re-read just the part you need \
with a narrow span, `cat -n FILE | sed -n '40,80p'`. Each call starts at the project root; \
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

/// The opening paragraph of the handshake: what kaibo is and the tool menu. Split out
/// so [`kaibo_instructions_with_scope`] can slot the `## Casts` roster *between* it and
/// `## Scope` — the menu of teams reads before scope, and both sit above the point a
/// truncating host (Claude Code's 2048-char cap) would cut.
fn kaibo_lead() -> &'static str {
    "kaibo (解剖) — grounded, cited answers about a codebase from a model outside \
     your own family. DeepSeek, Gemini, Anthropic, or a local model reads the \
     project READ-ONLY and answers with file:line citations. Say in prose what you \
     did or want to know — kaibo finds and reads the current code itself; no \
     pasted files or diffs needed. `consult` is the front door. `oneshot` is a \
     toolless second opinion when you own the context. `run_kaish` drives the \
     read-only shell directly. Work you don't wait on: `consult_submit` and \
     `batch_submit` return handles; `job_wait`/`job_get`/`job_list`/`job_cancel` \
     manage them."
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
         kaibo's default cast `{cast}` has no usable API key, so `consult`/`oneshot` \
         can't reach a model yet. `run_kaish` works now (read-only shell, no model), so \
         you can browse the code meanwhile.\n\n\
         Give the cast a key via an **environment variable** or **key file** — kaibo \
         reads either at startup, so it stays out of the chat (set it in your shell; \
         don't paste it to the model):\n\
         {backends}\n\n\
         Then **reconnect the kaibo MCP server** so it re-reads the environment — in \
         Claude Code run `/mcp`; other hosts have their own reload. Prefer another \
         provider? The annotated `kaibo://config/example` resource (→ \
         `~/.config/kaibo/config.toml`) shows every backend and cast.",
        cast = config.default_cast,
    )
}

/// The `## Casts` block: the casts that can reach a model *right now* (from
/// [`Config::usable_casts`]), each line naming the cast's answering (synth) model —
/// the team's voice, so an agent told "ask Gemini Pro" indexes the right cast — with
/// the default marked, a local/unverified one tagged, and a batch-only cast tagged
/// `batch` (it's the `batch_submit` lane). This is the handshake answering "what can I
/// pass as `cast`?" truthfully — it names config.toml casts the static per-tool `cast`
/// enum can't, and lists only what will actually work (an unconfigured cast is filtered
/// upstream). It closes by pointing at `kaibo://config` as canonical for the full
/// configured state, since this list is deliberately partial (usable-only) and read
/// once at startup. The synth model lives on the resolved `Config` already (it's what
/// `kaibo://config` prints) — this surfaces it where the calling agent first reads.
///
/// Empty `usable` (no cast can reach a model) renders nothing — the `Unconfigured`
/// setup banner already owns that case and would otherwise say it twice. Returns a
/// trailing `\n\n` so the caller can splice it in unconditionally.
fn casts_section(config: &Config, usable: &[(String, CastUsability)]) -> String {
    if usable.is_empty() {
        return String::new();
    }
    let lines: String = usable
        .iter()
        .map(|(name, state)| {
            let mut tags = Vec::new();
            if config.is_default_cast(name) {
                tags.push("default".to_string());
            }
            if matches!(state, CastUsability::LocalUnverified) {
                tags.push("local, unverified".to_string());
            }
            // A batch cast runs synth alone on the `batch_submit` lane (no explorer),
            // so tag it: the agent learns which tool the cast belongs to, not just its
            // name. `cast_is_batch` is the same predicate the per-lane enum split uses.
            if config.cast_is_batch(name) {
                tags.push("batch".to_string());
            }
            let suffix = if tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", tags.join(", "))
            };
            // Name the answering (synth) model — the team's voice, the thing an agent
            // told "ask Gemini Pro" indexes on. The data is already resolved on the
            // Config (it's what `kaibo://config` prints); a cast with no synth slot
            // (explorer-only) just renders its name. Resolution is structural, not
            // key-gated, so it holds for every usable cast.
            let synth = config.resolve_cast(name).ok().and_then(|cast| {
                cast.slot(ModelRole::Synth)
                    .map(|slot| format!("{}/{}", slot.backend, slot.id))
            });
            match synth {
                Some(model) => format!("- `{name}`{suffix} → {model}"),
                None => format!("- `{name}`{suffix}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## Casts\n\
         A cast is the model team that staffs a consultation; pass `cast=<name>`. \
         Usable right now (resolved at startup — reconnect after a config or key \
         change; `kaibo://config` lists every configured cast, not just these):\n\
         {lines}\n\n"
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
///
/// The resident handshake carries no kaish onboarding reference: Claude Code
/// hard-truncates a server's `instructions` at exactly 2048 characters (measured
/// live, per-server, hardcoded), and the onboarding spine blew that budget on its own
/// — before `## Scope`, the containment/trust posture, could render. The shell stays
/// reachable through `run_kaish`'s own description, the `kaibo://kaish/*` resources,
/// and `help` inside a script.
pub fn kaibo_instructions_with_scope(
    config: &Config,
    allowed_set: &[PathBuf],
    default_root: Option<&Path>,
    default_root_inferred: bool,
    usability: CastUsability,
    usable_casts: &[(String, CastUsability)],
) -> String {
    // The unconfigured-install banner leads, so a fresh user sees it first.
    let setup = match usability {
        CastUsability::Unconfigured => format!("{}\n\n", setup_section(config)),
        CastUsability::Ready | CastUsability::LocalUnverified => String::new(),
    };
    // Lead, then the live cast roster, then Scope directly — no kaish reference in
    // between. A caller's first decision is "which team"; Scope is the containment
    // posture every handshake must carry, so it now sits right after Casts instead
    // of below the reference wall a truncating host would drop it behind.
    let lead = kaibo_lead();
    let casts = casts_section(config, usable_casts);

    // Scope section: always accurate, never ambiguous. Report the *effective* default
    // root (an explicit `--root`, or the launch cwd kaibo inferred), and tag the
    // inferred case so the caller can tell it wasn't configured by hand.
    let root_line = match default_root {
        Some(r) if default_root_inferred => format!(
            "- **Default root:** `{}` (inferred from launch cwd — a call may omit `path`)",
            r.display()
        ),
        Some(r) => format!("- **Default root:** `{}`", r.display()),
        None => "- **Default root:** none — every call must pass a `path` argument.".to_string(),
    };
    let allowed_lines: String = allowed_set
        .iter()
        .map(|p| format!("  - `{}`", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{setup}{lead}\n\n\
         {casts}\
         ## Scope\n\
         Read-only, always: kaibo never writes and cannot run external commands. A \
         per-call `path` must canonicalize to at-or-under one of these allowed trees:\n\n\
         {root_line}\n\
         - **Allowed trees:**\n\
         {allowed_lines}\n\n\
         Go deeper without spending a turn: `kaibo://config` (full resolved config — \
         casts, backends, gated tools, sandbox limits), `kaibo://tools` (attachments, \
         overrides, the async workflow), `kaibo://kaish/*` (shell syntax and idioms)."
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
         Lead with line numbers so every claim cites `file:line`, and read \
         generously — favor a whole file over a narrow slice:\n\
         - `cat -n FILE` — a whole file with line numbers; reach for it first\n\
         - `grep -rn PATTERN [PATH]` — matches with line numbers, across files\n\
         - `grep -rn -B3 -A6 PATTERN .` — matches with the lines around them\n\
         - `grep -rl PATTERN src` — just the file names that match\n\
         - `cat -n FILE | sed -n '40,80p'` — a numbered span of a large file\n\n\
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
         `kaibo://kaish/builtin/grep`). All of it is also available inside a script: \
         `help`, `help syntax`, `help <builtin>`.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Cast, ModelSlot};

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
    fn lead_steers_callers_to_describe_intent_not_paste_a_diff() {
        // The handshake must teach the client agent that kaibo reads the real code
        // itself, so it should say what it did rather than dump a diff. If this
        // framing is ever dropped, callers fall back to pasting source kaibo would
        // only re-read from disk — catch that here.
        let lead = kaibo_lead().to_lowercase();
        assert!(
            lead.contains("diff"),
            "lead must steer callers away from pasting a diff:\n{}",
            kaibo_lead()
        );
        assert!(
            lead.contains("finds and reads the current code"),
            "lead must say kaibo finds and reads the code itself:\n{}",
            kaibo_lead()
        );
        // consult is the front door; the others surface via schema.
        assert!(
            lead.contains("`consult` is the front door"),
            "lead must foreground `consult` as the front door:\n{}",
            kaibo_lead()
        );
    }

    #[test]
    fn addendum_states_the_exit_code_contract_and_line_browsing() {
        // The two things the synth preamble rewards (exact file:line) and the two
        // codes an automated caller will misread without help. These are kaibo's,
        // so they live in the addendum (kaish-help can't know our 126/124).
        for needle in ["cat -n", "grep -rn", "126", "124"] {
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
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
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
                &config,
                &[PathBuf::from("/tmp")],
                None,
                false,
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
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
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
        // Points at the config resource for the full configured state — the roster
        // lists usable casts only, and `kaibo://config` has every one (surfaced in
        // the Casts aside and, authoritatively, in the Scope "go deeper" pointers).
        assert!(
            text.contains("kaibo://config"),
            "handshake must point at kaibo://config for the full configured state:\n{text}"
        );
    }

    /// Each roster line names the cast's **answering (synth) model**, so an agent told
    /// "ask Gemini Pro" can index `gemini-batch → …/gemini-pro-latest` straight from the
    /// handshake without re-reading `kaibo://config`. A batch cast (synth-only, a
    /// different tool/lane) is tagged `batch` so the agent picks the right one. The data
    /// is already on the resolved `Config` — this just renders it.
    #[test]
    fn casts_section_names_each_synth_model_and_tags_batch() {
        let config = Config::builtin(); // built-in casts: anthropic, gemini-batch, …
        let usable = vec![
            ("anthropic".to_string(), CastUsability::Ready),
            ("gemini-batch".to_string(), CastUsability::Ready),
        ];
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
            CastUsability::Ready,
            &usable,
        );
        // The interactive cast names its synth (Claude Sonnet, the built-in anthropic synth).
        assert!(
            text.contains("anthropic/claude-sonnet-4-6"),
            "roster must name the anthropic cast's synth model:\n{text}"
        );
        // The batch cast names Gemini Pro — the whole point: Pro is reachable only here.
        assert!(
            text.contains("gemini/gemini-pro-latest"),
            "roster must name gemini-batch's synth (Gemini Pro):\n{text}"
        );
        // ...and the line is tagged `batch`, so the agent knows it's the batch_submit lane.
        let pro_line = text
            .lines()
            .find(|l| l.contains("gemini-batch"))
            .expect("gemini-batch has a roster line");
        assert!(
            pro_line.contains("batch"),
            "the gemini-batch line must carry a batch tag:\n{pro_line}"
        );
    }

    /// The `(default)` tag survives a default cast set by *alias*. `usable_casts`
    /// yields canonical names (`anthropic`), but an operator may write
    /// `server.cast = "claude"` (an alias) — a raw `name == default_cast` would compare
    /// `"anthropic" == "claude"`, miss, and silently drop the tag. The roster must
    /// resolve the default before comparing. (Reviewer-found: the alias/default
    /// equality bug, latent in the bare-string compare.)
    #[test]
    fn casts_section_marks_the_default_even_when_set_by_alias() {
        let mut config = Config::builtin();
        config.default_cast = "claude".to_string(); // alias → canonical `anthropic`
        let usable = vec![("anthropic".to_string(), CastUsability::Ready)];
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
            CastUsability::Ready,
            &usable,
        );
        let line = text
            .lines()
            .find(|l| l.contains("anthropic"))
            .expect("anthropic has a roster line");
        assert!(
            line.contains("(default)"),
            "the canonical cast of an alias default must still be tagged default:\n{line}"
        );
    }

    /// An explorer-only cast (no synth slot) renders its name with no `→ model` — the
    /// handshake doesn't invent an answerer it can't name (the synth gap surfaces at
    /// call time). Exercises the `None` arm DeepSeek flagged as uncovered.
    #[test]
    fn casts_section_renders_a_synthless_cast_as_name_only() {
        let config = Config::builtin();
        // A name absent from the registry resolves to no synth slot — the same render
        // path a real explorer-only cast takes, without fabricating one in the config.
        let usable = vec![("explorer-only".to_string(), CastUsability::Ready)];
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
            CastUsability::Ready,
            &usable,
        );
        let line = text
            .lines()
            .find(|l| l.contains("explorer-only"))
            .expect("explorer-only has a roster line");
        assert!(
            !line.contains('→'),
            "a cast with no synth slot must not render an arrow/model:\n{line}"
        );
    }

    /// The resident handshake dropped the huge kaish onboarding reference entirely —
    /// it blew Claude Code's 2048-char instructions budget and buried `## Scope`
    /// below the truncation point. Order is now lead → casts → scope, and the old
    /// reference marker ("The shell is kaish") must not appear at all.
    #[test]
    fn scope_follows_casts_and_the_kaish_reference_is_gone() {
        let config = Config::builtin();
        let usable = vec![("anthropic".to_string(), CastUsability::Ready)];
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
            CastUsability::Ready,
            &usable,
        );
        let casts_at = text.find("## Casts").expect("has a Casts section");
        let scope_at = text.find("## Scope").expect("has a Scope section");
        let lead_at = text.find("kaibo (解剖)").expect("opens with the lead");
        assert!(
            lead_at < casts_at && casts_at < scope_at,
            "order must be lead → casts → scope (got lead={lead_at}, casts={casts_at}, \
             scope={scope_at}):\n{text}"
        );
        assert!(
            !text.contains("The shell is kaish"),
            "the resident handshake must drop the kaish onboarding reference \
             entirely — it no longer fits the truncation budget:\n{text}"
        );
    }

    /// Claude Code truncates a server's MCP `instructions` at exactly 2048
    /// characters — measured live against a running server; it's a per-server,
    /// hardcoded client-side cap, not an MCP-spec limit and not configurable. Past
    /// that boundary the calling model never sees the rest, which is exactly how
    /// `## Scope` — the containment/trust posture — used to go missing behind the
    /// huge kaish onboarding reference. This drives a *representative* roster (10
    /// casts: the 6 built-ins plus 4 more spanning default/local-unverified/batch)
    /// through the full resident handshake and asserts it fits. Fails against
    /// today's layout (the resident kaish reference blows the budget on its own).
    #[test]
    fn instructions_fit_claude_code_budget() {
        let mut config = Config::builtin(); // anthropic, deepseek, gemini,
                                             // openai-local, gemini-batch, anthropic-batch
        for (name, backend, id) in [
            ("chimera", "anthropic", "claude-haiku-4-5"),
            ("glm", "openai-local", "GLM-4.5-Air-UD-Q4K-XL-GGUF"),
            ("qwen", "openai-local", "Qwen3-Coder-Next-GGUF"),
            ("zorak", "openai-local", "gemma4-26b"),
        ] {
            config.casts.insert(
                name.to_string(),
                Cast {
                    name: name.to_string(),
                    slots: std::collections::BTreeMap::from([(
                        ModelRole::Synth,
                        ModelSlot::bare(backend, id),
                    )]),
                    batch: false,
                },
            );
        }

        // A realistic usable-casts mix: the interactive built-ins, both batch
        // lanes (tagged `batch` off the config's own `batch` flag), and a spread
        // of local/unverified entries — 10 lines total, not the 6-cast minimum.
        let usable = vec![
            ("anthropic".to_string(), CastUsability::Ready),
            ("deepseek".to_string(), CastUsability::Ready),
            ("gemini".to_string(), CastUsability::Ready),
            ("gemini-batch".to_string(), CastUsability::Ready),
            ("anthropic-batch".to_string(), CastUsability::Ready),
            ("chimera".to_string(), CastUsability::Ready),
            ("glm".to_string(), CastUsability::LocalUnverified),
            ("qwen".to_string(), CastUsability::LocalUnverified),
            ("zorak".to_string(), CastUsability::LocalUnverified),
            ("openai-local".to_string(), CastUsability::LocalUnverified),
        ];

        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/home/amy/src/some-project")],
            Some(Path::new("/home/amy/src/some-project")),
            true,
            CastUsability::Ready,
            &usable,
        );

        let len = text.chars().count();
        assert!(
            len < 2048,
            "handshake must fit Claude Code's 2048-char instructions budget \
             (measured live, per-server, hardcoded), got {len} chars:\n{text}"
        );
    }

    /// The *unconfigured* (fresh-install) handshake must fit the same 2048-char budget:
    /// it prepends [`setup_section`] (per-backend key instructions) and renders no cast
    /// roster, so it's a different budget shape than the configured case above — and the
    /// same failure mode (a truncating host drops `## Scope`) applies. Uses the built-in
    /// default cast (`anthropic`, one backend), the real fresh-install scenario. A custom
    /// default cast naming many backends could still overflow — that's an operator edge
    /// we can't bound here — but the shipped default must fit.
    #[test]
    fn unconfigured_instructions_fit_claude_code_budget() {
        let config = Config::builtin(); // default cast "anthropic", one backend
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/home/amy/src/some-project")],
            Some(Path::new("/home/amy/src/some-project")),
            true,
            CastUsability::Unconfigured,
            &[], // nothing usable yet — the setup banner owns this case
        );
        let len = text.chars().count();
        assert!(
            text.contains("Setup needed"),
            "the unconfigured handshake must lead with the setup banner:\n{text}"
        );
        assert!(
            len < 2048,
            "the fresh-install handshake must fit the 2048-char budget too, got \
             {len} chars:\n{text}"
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
