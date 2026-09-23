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

use crate::config::{CastUsability, Config, Lane, ModelRole};

/// The kaibo-specific half of the core: the read-only boundary, the exit-code
/// contract, the no-cwd rule, and the line-number idioms that make citations
/// exact. These are *not* in `kaish-help` — they describe kaibo's sandbox, not
/// kaish the language — so they're authored here and layered onto the canonical
/// contract. Positive framing on purpose (weaker/local models loop on blanket
/// prohibitions): say what reading looks like, rather than listing what is refused.
/// Plain and literal per the agent-facing clarity rule in AGENTS.md — this block is
/// embedded in every preamble, so an idiom here is charged to every model we drive.
///
/// The grep idiom is written **without an operand**, and that is deliberate. kaish
/// 0.16 made `grep -r` prefix each hit with the operand as written, matching GNU: an
/// explicit `.` yields `./src/foo.rs:12`, a named file yields no path at all, and only
/// a *defaulted* operand yields the bare `src/foo.rs:12` a citation wants. Since every
/// call starts at the project root, the operand kaibo used to teach only ever added
/// two characters to every citation the explorer earns. The old sentence also promised
/// the idiom worked "whether the target is a file or a directory" — the file case
/// drops the filename, which is the half a citation needs, so the promise went with it.
///
/// Every `sed` range here is written **quoted**, but the addendum no longer explains
/// why, because the reason stopped being true. kaish reserved a bare comma through
/// 0.13, making `sed -n 120,400p` a parse error; kaish 0.14 made a comma an ordinary
/// bareword character outside `[...]`/`{...}`, so both forms parse now. The quoted
/// examples stay — they are correct on every version kaibo has ever run — and the
/// stated rule went, since a model reads this block every turn and a false reason is
/// worse than a missing one.
pub const KAISH_SANDBOX_ADDENDUM: &str = "\
In kaibo this shell runs over a READ-ONLY snapshot of one project, offline: writes, \
`git`, `touch`, and external commands are refused, so your work here is reading. Read \
files WHOLE by default with `cat -n FILE`; `grep -rn PATTERN` searches the \
whole project and prefixes every hit with its path from the root. PATTERN is a \
regex; search literal text with `-F`, as in `grep -rnF 'fn consult(' src`. When a \
grep hit lands in a large file, read a \
wide span around it with `cat -n FILE | sed -n '120,400p'`, which returns that range \
with its real line numbers. Run `file FILE` on an unfamiliar file first; it names \
the content as text or binary, so you know what you are about to read. \
Each call starts at the project root; \
there is no persistent cwd. Read the exit code: 0 is success; -1 means kaish could \
not run the script (a parse or validation failure, where nothing ran, or a shell error \
partway through), and stderr says why; 3 means the output \
was too large and came back as a head+tail sample (not a failure); 124 means the \
script was killed for running past its time budget; 127 is how every external \
command answers here — its message names the refusal, as in `curl: external \
commands are not available in this build of the shell`; 1 is an ordinary failure, \
and a refused \
write is one of those — its message reads `permission denied: filesystem is \
read-only`. Read the message and not only the code, because that sentence is what \
tells a refusal apart from a mistake. \
To learn more, run `help`, `help syntax`, or `help <builtin>` in any \
script, or read the `kaibo://kaish/*` resources.";

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
///
/// kaibo carried a `strip_write_side_paragraphs` filter here until kaish 0.14: the
/// shared contract used to teach "Overlay mode" (a virtual *write* layer and
/// `kaish-vfs commit`) to embedders in general, which is dead weight in a kaibo
/// preamble and a mixed signal one paragraph before "writes are refused". kaish-help
/// #297 made that guidance opt-in (`Concept::Overlay`, reached only through
/// `Selector::with_overlay`), so the paragraph no longer arrives and the filter had
/// nothing left to remove. kaibo never opts in; `core_carries_no_write_side_teaching`
/// is what keeps that true.
pub fn kaish_syntax_core() -> &'static str {
    static CORE: OnceLock<String> = OnceLock::new();
    CORE.get_or_init(|| format!("{}\n\n{}", kaish_operating_contract(), KAISH_SANDBOX_ADDENDUM))
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
/// so [`kaibo_instructions_with_scope`] can compose it with `## Scope` and the
/// `## Casts` roster, in that order. The roster goes last because it is the only
/// section whose size the operator controls, so a truncating host (Claude Code's
/// 2048-char cap) cuts it rather than Scope.
fn kaibo_lead() -> &'static str {
    "kaibo — codebase review and second opinions from another model family. \
     Hosted casts send questions, context, and source to configured providers; \
     local casts use local endpoints. The project stays READ-ONLY. `consult` is the front door; kaibo \
     finds and reads the current code, then answers with file:line citations. \
     Say what you did or want to know; no pasted files or diffs needed. \
     `explore` returns a cited survey report. `oneshot` answers from supplied context \
     with no codebase access. `run_kaish` drives the read-only shell directly. \
     `deliberate` reasons offline over a dossier. `consult_submit` and `batch_submit` \
     return handles; `job_wait`/`job_get`/`job_list`/`job_cancel` manage them."
}

/// The setup-guidance block prepended to the instructions when the default cast has
/// no usable provider (a fresh `cargo install` with no key source declared). Steers
/// toward DECLARING a key source in config.toml (api_key_env / api_key_file /
/// api_key_cmd) — *never* pasting the key into the chat — names the default cast's
/// backends and whatever source each already carries (else the kind's conventional
/// env var, since conventions are exactly what guidance may name), points at the
/// example resource, and reminds the user to reconnect the server (which only
/// re-reads the environment and config at startup).
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
                    // Name what's already declared (and why it's still missing), else
                    // the kind's conventional env var — the line an operator can act on.
                    let declared = match (&b.api_key_env, &b.api_key_file, &b.api_key_cmd) {
                        (Some(env), _, _) => format!("declared env `{env}` is unset"),
                        (None, Some(f), _) => format!("declared key file `{f}` is absent"),
                        // A cmd-backed backend is always `key_status` Present (never
                        // run to classify), so this arm renders only in a partial-setup
                        // cast (some other slot's backend is Missing) — and the message
                        // stays neutral: the command itself is fine, it just has not
                        // run yet.
                        (None, None, Some(_)) => "declared key command (runs on first use)".into(),
                        (None, None, None) => {
                            format!("declare `api_key_env = \"{}\"`", b.kind.env_var())
                        }
                    };
                    lines.push(format!(
                        "  - backend `{}` ({}) — {declared}",
                        b.name,
                        b.kind.canonical_name(),
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
         can't reach a model — but `run_kaish` works now (read-only shell, no model) \
         to browse the code meanwhile.\n\n\
         Key sources are declared, never assumed: add one to config.toml — an env var \
         name (`api_key_env`), a key file path (`api_key_file`), or a command that \
         prints the key (`api_key_cmd`). The value lives outside the config, so the \
         secret stays out of the chat (set it in your shell, never paste it here):\n\
         {backends}\n\n\
         Then **reconnect the kaibo MCP server** so it re-reads config and env \
         (Claude Code: `/mcp`). `kaibo example-config` prints the template \
         (`kaibo://config/example`).",
        cast = config.default_cast,
    )
}

/// How many allowed trees `## Scope` lists before it summarizes the rest in one
/// `+N more` line. The allowed set is the root, each `--allow-path`, and the launch cwd,
/// with no cap, so a line per tree could push Scope itself past Claude Code's 2048-char
/// cut. `kaibo://config` lists every tree. `lead_and_scope_fit_the_budget_at_any_
/// allowed_tree_count` holds the bound.
const ALLOWED_TREE_MAX_LINES: usize = 4;

/// How many casts the resident roster names before it summarizes the rest in one
/// `+N more` line. The roster is the one handshake section whose size the operator
/// controls, so it is the one that gets a fixed cap: a 26-cast config rendered 2,596
/// characters against Claude Code's 2048. Eight lines of typical length plus the lead
/// and `## Scope` leave room for a few allowed trees; `instructions_fit_claude_code_
/// budget_at_any_roster_size` holds the total.
const ROSTER_MAX_LINES: usize = 8;

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
/// The roster names at most [`ROSTER_MAX_LINES`] casts, the default first so it always
/// makes the list, and then says how many it left out. It renders last in the
/// handshake, after `## Scope`, so a roster longer than the budget expects cuts into
/// its own tail and never into the containment posture.
///
/// Empty `usable` (no cast can reach a model) renders nothing — the `Unconfigured`
/// setup banner already owns that case and would otherwise say it twice.
fn casts_section(config: &Config, usable: &[(String, CastUsability)]) -> String {
    if usable.is_empty() {
        return String::new();
    }
    // The default leads; the rest keep the caller's (alphabetical) order.
    let (default, rest): (Vec<_>, Vec<_>) =
        usable.iter().partition(|(name, _)| config.is_default_cast(name));
    let mut lines: Vec<String> = default
        .into_iter()
        .chain(rest)
        // A `direct`-lane cast is kept out of this *resident* roster to hold the 2048-char
        // budget: `deliberate` now routes to it (its `cast` enum lists the deliberate-usable
        // direct casts authoritatively, and `kaibo://config` renders every cast), so it's
        // not unadvertised — just not in the always-billed handshake summary, which stays
        // synth-voice-focused. The batch deliberate casts still appear here (tagged `batch`).
        .filter(|(name, _)| config.cast_offline_lane(name) != Some(Lane::Direct))
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
            // Name the answering (synth) model — the team's voice, the thing an agent
            // told "ask Gemini Pro" indexes on. The data is already resolved on the
            // Config (it's what `kaibo://config` prints). Resolution is structural, not
            // key-gated, so it holds for every usable cast.
            //
            // A cast with no synth answers no text tool, so its line says which tool it
            // does serve (`generate` for an image slot, `explore` for an explorer slot)
            // and names that slot's model instead. A bare name there read as a team
            // whose answering model was merely unknown. A name that does not resolve
            // still renders bare.
            let cast = config.resolve_cast(name).ok();
            let model_slot = cast.and_then(|cast| {
                if let Some(slot) = cast.slot(ModelRole::Synth) {
                    return Some(slot);
                }
                let serves: Vec<&str> = [
                    (ModelRole::Explorer, "`explore`"),
                    (ModelRole::Image, "`generate`"),
                ]
                .into_iter()
                .filter(|(role, _)| cast.slot(*role).is_some())
                .map(|(_, tool)| tool)
                .collect();
                if !serves.is_empty() {
                    tags.push(format!("{} only", serves.join(" or ")));
                }
                cast.slot(ModelRole::Image)
                    .or_else(|| cast.slot(ModelRole::Explorer))
            });
            let suffix = if tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", tags.join(", "))
            };
            match model_slot {
                Some(slot) => format!("- `{name}`{suffix} → {}/{}", slot.backend, slot.id),
                None => format!("- `{name}`{suffix}"),
            }
        })
        .collect();
    if lines.len() > ROSTER_MAX_LINES {
        let left_out = lines.len() - ROSTER_MAX_LINES;
        lines.truncate(ROSTER_MAX_LINES);
        lines.push(format!("- +{left_out} more: `kaibo://config`"));
    }
    let lines = lines.join("\n");

    format!(
        "## Casts\n\
         A cast is the model team that answers; pass `cast=<name>`. Usable now, as \
         resolved at startup (reconnect after a config or key change). \
         `kaibo://config` lists every configured cast:\n\
         {lines}"
    )
}

/// Build the MCP server instructions, with a **scope section** appended so the calling
/// model always knows:
/// - the default root (or that every call must pass one),
/// - the allowed trees a per-call `path` must be at-or-under, and
/// - that `kaibo://config` has the full picture.
///
/// When `usability` is [`CastUsability::Unconfigured`] (a fresh install with no key),
/// a [`setup_section`] is prepended so the calling model can walk the user through
/// configuration. `Ready`/`LocalUnverified` get the normal instructions unchanged.
///
/// Used by `get_info` so every `initialize` handshake and `server/discover` surfaces the server's
/// containment posture. Unit-testable: pass your own `Config`, `allowed_set`, and
/// `usability` rather than fabricating a `RequestContext` or reading the environment.
///
/// The resident handshake carries no kaish onboarding reference: Claude Code
/// hard-truncates a server's `instructions` at exactly 2048 characters (measured
/// live, per-server, hardcoded), and the onboarding spine blew that budget on its own
/// — before `## Scope`, the containment/trust posture, could render. The shell stays
/// reachable through `run_kaish`'s own description, the `kaibo://kaish/*` resources,
/// and `help` inside a script.
///
/// Dropping the reference did not settle the budget for good. The cast roster grew one
/// line per usable cast and pushed `## Scope` past the cut again (26 casts, 2,596
/// characters). What holds it now: Scope renders before the roster, the roster is
/// capped at [`ROSTER_MAX_LINES`], and Scope's allowed-tree list at
/// [`ALLOWED_TREE_MAX_LINES`]. Every line count is bounded. What stays unbounded is the
/// length of operator text inside those lines: the default-root and allowed-tree paths
/// (which can push Scope itself past the cut), and cast names and model ids (which can
/// push only the roster's tail, since it renders last). The unconfigured case adds the
/// setup banner, which names the default cast's backends.
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
    // Lead, then Scope, then the live cast roster. Scope is the containment posture
    // every handshake must carry and its size is fixed by kaibo; the roster's size is
    // the operator's, so it goes last, where a truncating host cuts it and not Scope.
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
    let mut allowed_lines: Vec<String> = allowed_set
        .iter()
        .take(ALLOWED_TREE_MAX_LINES)
        .map(|p| format!("  - `{}`", p.display()))
        .collect();
    if allowed_set.len() > ALLOWED_TREE_MAX_LINES {
        allowed_lines.push(format!(
            "  - +{} more: `kaibo://config`",
            allowed_set.len() - ALLOWED_TREE_MAX_LINES
        ));
    }
    let allowed_lines = allowed_lines.join("\n");
    // A linked git worktree of an allowed tree is in scope too — kaibo vouches for it
    // by reading git's own link files, never by trusting the candidate's `.git`. Only
    // said when the (default-on) feature is actually live, so a `--no-follow-worktrees`
    // server doesn't claim a boundary it isn't enforcing. Dropped in the Unconfigured
    // case: the setup banner already owns that budget (see `unconfigured_instructions_
    // fit_claude_code_budget`), and a fresh install with no key isn't calling `consult`
    // against a worktree yet anyway.
    let worktree_note = if config.follow_worktrees && usability != CastUsability::Unconfigured {
        " (a git worktree of one counts too)"
    } else {
        ""
    };

    // `casts` is empty in the Unconfigured case; it is the last section otherwise.
    let casts = if casts.is_empty() {
        String::new()
    } else {
        format!("\n\n{casts}")
    };
    format!(
        "{setup}{lead}\n\n\
         ## Scope\n\
         Read-only, always: kaibo never writes your project, and runs only the \
         key-fetch command your config declares (`api_key_cmd`, stdin closed, never \
         logged). A \
         per-call `path` must canonicalize to at-or-under one of these allowed \
         trees{worktree_note}:\n\n\
         {root_line}\n\
         - **Allowed trees:**\n\
         {allowed_lines}\n\n\
         More without spending a turn: `kaibo://config`, `kaibo://tools`, \
         `kaibo://kaish/*`.{casts}"
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
         Lead with line numbers so every claim cites `file:line`, and read files \
         whole:\n\
         - `cat -n FILE` — the whole file, numbered; the default move on any file that matters\n\
         - `grep -rn PATTERN` — find which files matter, then open them whole; every hit is prefixed with its path from the project root\n\
         - `grep -rn -B3 -A6 PATTERN` — preview matches in context across files\n\
         - `grep -rn PATTERN DIR/` — narrow to a subtree; hits are prefixed with the operand as written, so a named directory still cites a usable path\n\
         - `grep -rl PATTERN src` — just the file names that match\n\
         - `grep -rnF 'fn consult(' src` — a literal search; PATTERN is otherwise a regex, \
         and an unbalanced `(` fails validation with exit `-1`\n\
         - `cat -n FILE | sed -n '1200,2400p'` — a targeted wide span of a truncated giant (`grep -n SYMBOL FILE` pins where to aim), and the follow-up to a grep hit in a large file\n\
         - `file FILE` — what a file is, text or binary, read from its content rather than its name\n\n\
         ## Read-only boundary\n\
         The project is mounted read-only and external commands are off, by \
         construction. Writes, `git`, `touch`, `spawn`/`exec`, and any external \
         command are refused. This is the product: read freely, expect no write to \
         land.\n\n\
         ## Exit codes\n\
         The code tells you the shape of the outcome; the message tells you which \
         outcome it was. A refused write and an ordinary mistake both exit `1`, so \
         read the stderr line: a refusal says `permission denied: filesystem is \
         read-only`.\n\
         - `0` — success\n\
         - `-1` — kaish could not run the script. Either it failed to parse or \
         validate, so nothing ran, or the shell hit an error partway through and the \
         output is dropped; stderr says why. The common cause is a grep pattern with a \
         literal `(`: search it with `grep -rnF`\n\
         - `1` — the command failed. A refused write is one of these, and its message \
         reads `permission denied: filesystem is read-only`\n\
         - `3` — output exceeded the cap and was truncated to a head+tail sample \
         (not a failure; the full output is not returned)\n\
         - `124` — killed for exceeding the per-exec time budget\n\
         - `126` — a builtin the operator disabled in kaibo's config; the default \
         config disables none, so you will rarely see this\n\
         - `127` — every external command answers this way, and its message names \
         the refusal (`curl: external commands are not available in this build of \
         the shell`), which is what makes the host unreachable from here\n\
         - `130` — the script was cancelled\n\
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
        // its load-bearing rules must survive the write-side filter. This fails the
        // moment the compose recipe breaks, the layering drops it, or the filter
        // over-strips.
        let contract = kaish_operating_contract();
        assert!(
            !contract.is_empty(),
            "the canonical contract must compose to something"
        );
        for rule in ["No word splitting", "Strict globs", "Pre-validation"] {
            assert!(
                contract.contains(rule) && core.contains(rule),
                "canonical rule {rule:?} must reach the core"
            );
        }
        // The kaibo half must be there too.
        assert!(
            core.contains(KAISH_SANDBOX_ADDENDUM),
            "core must embed the kaibo sandbox addendum verbatim"
        );
    }

    /// kaibo's core is a READ-ONLY surface, so write-side teaching must not reach
    /// it: "Overlay mode" is a virtual write layer (`kaish-vfs commit`), dead weight
    /// in a kaibo preamble and a mixed signal one paragraph before "writes are
    /// refused".
    ///
    /// The property is unchanged from the version of this test that guarded kaibo's
    /// own `strip_write_side_paragraphs` filter; only the mechanism moved. Since
    /// kaish-help #297 the guidance is opt-in (`Selector::with_overlay`) and kaibo
    /// never opts in, so upstream's default keeps the paragraph out and the filter
    /// was retired in the 0.14 bump. This still fails if a future kaish makes
    /// overlay guidance default-on, or if a kaibo recipe ever opts in — which is
    /// exactly why it outlived the filter it was written for.
    #[test]
    fn core_carries_no_write_side_teaching() {
        let core = kaish_syntax_core();
        for needle in ["Overlay mode", "kaish-vfs commit"] {
            assert!(
                !core.contains(needle),
                "write-side teaching {needle:?} must not reach kaibo's core:\n{core}"
            );
        }
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

    /// The two things the synth preamble rewards (exact `file:line`) and the exit codes
    /// an automated caller will misread without help. These are kaibo's own, so they live
    /// in the addendum — kaish-help cannot know them.
    ///
    /// The negative assertion is the load-bearing one. Until 2026-08-13 all six of kaibo's
    /// exit-code descriptions said `126` meant "blocked by the read-only sandbox". Nothing
    /// emits 126 for a refused write: the read-only mount is structural and refuses with
    /// the VFS's own `1` (`a14c7e7` moved it there in June 2026, and the prose never
    /// followed), while 126 belongs to an operator-disabled builtin that the default
    /// config never produces. A model told to branch on 126 to detect a refusal would
    /// never detect one, and would read `1` as its own command failing. So this pins the
    /// true discriminator — the message — and pins that the false code cannot come back.
    #[test]
    fn addendum_states_the_exit_code_contract_and_line_browsing() {
        for needle in [
            "cat -n",
            "grep -rn",
            "124",
            "127",
            "permission denied: filesystem is read-only",
            // 4.1% of 11,195 `run_kaish` calls returned -1, most often a grep pattern
            // with a literal `(`; nothing in kaibo's text named the code.
            "-1 means kaish could not run the script",
            "a parse or validation failure, where nothing ran, or a shell error partway \
             through",
            "`grep -rnF 'fn consult(' src`",
        ] {
            assert!(
                KAISH_SANDBOX_ADDENDUM.contains(needle),
                "addendum must mention {needle:?}"
            );
        }
        assert!(
            !KAISH_SANDBOX_ADDENDUM.contains("126"),
            "the addendum must not teach 126 as the blocked code — a refused write exits 1 \
             and names itself, and 126 only fires for an operator-disabled builtin:\n{}",
            KAISH_SANDBOX_ADDENDUM
        );
    }

    /// The two reading idioms that follow a grep hit: a wide span around a match in a
    /// large file, and `file` to learn what a file is before reading it. Whole-file
    /// reading stays the default and is asserted next door; these are the follow-ups.
    ///
    /// The span is pinned quoted because that form is correct on every kaish kaibo has
    /// run. What this test deliberately no longer asserts is the *rule* — kaish 0.14
    /// made a bare comma an ordinary bareword, so an unquoted range parses too, and an
    /// assertion demanding kaibo teach otherwise would pin a false sentence into every
    /// preamble. Quoting is now a house convention, not a requirement.
    #[test]
    fn addendum_teaches_quoted_spans_and_identifying_a_file() {
        let a = KAISH_SANDBOX_ADDENDUM;
        assert!(
            a.contains("`cat -n FILE | sed -n '120,400p'`"),
            "the addendum must teach a wide span around a grep hit in a large file:\n{a}"
        );
        assert!(
            !a.contains("unquoted comma"),
            "the addendum must not explain quoting by a kaish 0.13 parse rule — kaish 0.14 \
             made a bare comma ordinary, so that reason is false and a model reads it every \
             turn:\n{a}"
        );
        assert!(
            a.contains("`file FILE`"),
            "the addendum must teach `file FILE` for an unfamiliar file, so a binary is \
             identified rather than discovered by reading it:\n{a}"
        );
        // Structural, so it survives someone changing the example line numbers: every
        // `sed -n` kaibo writes here is followed by a quote. The count is asserted too —
        // a loop over zero matches passes green, so without this the whole scan would go
        // quiet the day someone removed the last span example (caught in review, and the
        // sibling scan in `prompts.rs` already had it).
        let mut seen = 0;
        for (i, _) in a.match_indices("sed -n ") {
            let rest = &a[i + "sed -n ".len()..];
            assert!(
                rest.starts_with('\''),
                "every `sed -n` range must be quoted — unquoted, kaish 0.13 refuses the \
                 comma with a parse error:\n{a}"
            );
            seen += 1;
        }
        assert!(
            seen > 0,
            "the addendum must carry at least one `sed -n` span for the scan above to \
             mean anything:\n{a}"
        );
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

    /// A `direct`-lane cast is forward-declared (validated, rendered on
    /// `kaibo://config`) but no tool routes to it yet, so the handshake roster must not
    /// advertise it — an agent reading `## Casts` should never see a cast every tool
    /// refuses. Mirrors the `direct`-lane exclusion from both `inject_cast_enum`
    /// partitions in `server.rs`.
    #[test]
    fn casts_section_excludes_a_direct_lane_cast() {
        let config = Config::from_toml_str(
            r#"
            [casts.mydirect]
            synth = { backend = "openai-local", id = "big-local-model", lane = "direct" }
            "#,
        )
        .unwrap();
        let usable = vec![
            ("anthropic".to_string(), CastUsability::Ready),
            ("mydirect".to_string(), CastUsability::LocalUnverified),
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
            text.contains("anthropic"),
            "the interactive cast must still render:\n{text}"
        );
        assert!(
            !text.contains("mydirect"),
            "a direct-lane cast must not appear in the roster (no tool routes to it \
             yet):\n{text}"
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

    /// A cast with no synth slot cannot answer `consult` or `oneshot`, so its roster
    /// line says which tool it serves and names that slot's model. An image-only cast
    /// used to render as a bare name, which read as a cast whose answering model was
    /// merely unknown.
    #[test]
    fn casts_section_tags_a_synthless_cast_with_the_tool_it_serves() {
        let mut config = Config::builtin();
        for (name, role, backend, id) in [
            ("flux", ModelRole::Image, "bfl", "flux-2-pro"),
            ("scout", ModelRole::Explorer, "deepseek", "deepseek-flash"),
        ] {
            config.casts.insert(
                name.to_string(),
                Cast {
                    name: name.to_string(),
                    slots: std::collections::BTreeMap::from([(
                        role,
                        ModelSlot::bare(backend, id),
                    )]),
                },
            );
        }
        let usable = vec![
            ("flux".to_string(), CastUsability::Ready),
            ("scout".to_string(), CastUsability::Ready),
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
            text.contains("- `flux` (`generate` only) → bfl/flux-2-pro"),
            "an image-only cast names `generate` and its image model:\n{text}"
        );
        assert!(
            text.contains("- `scout` (`explore` only) → deepseek/deepseek-flash"),
            "an explorer-only cast names `explore` and its explorer model:\n{text}"
        );
    }

    /// The resident handshake dropped the huge kaish onboarding reference entirely —
    /// it blew Claude Code's 2048-char instructions budget and buried `## Scope`
    /// below the truncation point. The roster then grew into the same failure, so the
    /// order is now lead → scope → casts: the fixed-size containment posture sits above
    /// the one section whose size the operator controls. The old reference marker
    /// ("The shell is kaish") must not appear at all.
    #[test]
    fn scope_precedes_casts_and_the_kaish_reference_is_gone() {
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
        // Assert the lead actually OPENS the text, rather than merely finding the first
        // "kaibo" somewhere before `## Casts` — the Casts section's own `kaibo://config`
        // reference would satisfy a bare `find`, leaving this weaker than it reads.
        assert!(
            text.starts_with(kaibo_lead()),
            "the instructions must open with the lead verbatim — it is the resident pitch \
             and the tool-search retrieval index:\n{text}"
        );
        assert!(
            scope_at < casts_at,
            "order must be lead → scope → casts (got scope={scope_at}, casts={casts_at}):\n{text}"
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
    /// that boundary the calling model never sees the rest.
    ///
    /// The roster is the one section whose size the operator controls: every usable
    /// cast used to add a line. A 26-cast config (Amy's, 2026-09-23) rendered 2,596
    /// characters, and `## Scope` sat past the cut. So this grows the roster one cast at
    /// a time, well past the point it once overflowed, and requires the whole text to
    /// fit and to carry Scope at every size. The model ids are long OpenRouter-style
    /// slugs, so each line costs about what a real one does.
    #[test]
    fn instructions_fit_claude_code_budget_at_any_roster_size() {
        let mut config = Config::builtin();
        config.default_cast = "deepseek".to_string();
        let mut usable = vec![("deepseek".to_string(), CastUsability::Ready)];
        for n in 0..40 {
            let name = format!("team-{n:02}-review");
            config.casts.insert(
                name.clone(),
                Cast {
                    name: name.clone(),
                    slots: std::collections::BTreeMap::from([(
                        ModelRole::Synth,
                        ModelSlot::bare("openrouter", "~google/gemini-flash-latest"),
                    )]),
                },
            );
            let state = if n % 3 == 0 {
                CastUsability::LocalUnverified
            } else {
                CastUsability::Ready
            };
            usable.push((name, state));
            usable.sort_by(|a, b| a.0.cmp(&b.0));

            let text = kaibo_instructions_with_scope(
                &config,
                &[
                    PathBuf::from("/home/amy/src/some-project"),
                    PathBuf::from("/home/amy/src/wt"),
                ],
                Some(Path::new("/home/amy/src/some-project")),
                true,
                CastUsability::Ready,
                &usable,
            );
            let len = text.chars().count();
            assert!(
                len <= 2048,
                "with {} usable casts the handshake is {len} chars, over Claude Code's \
                 2048-char instructions budget:\n{text}",
                usable.len()
            );
            assert!(
                text.contains("## Scope") && text.contains("Read-only, always"),
                "with {} usable casts the handshake lost `## Scope`:\n{text}",
                usable.len()
            );
            assert!(
                text.contains("`deepseek` (default)"),
                "the default cast must always make the roster:\n{text}"
            );
        }
    }

    /// The allowed set has one entry per `--allow-path` plus the root and the launch
    /// cwd, with no cap, and Scope used to print a line for each. Enough of them pushed
    /// Scope past Claude Code's 2048-character cut the same way the roster once did.
    /// This grows the allowed set one tree at a time and requires the lead plus Scope,
    /// the part a caller must always see, to fit on its own at every size.
    #[test]
    fn lead_and_scope_fit_the_budget_at_any_allowed_tree_count() {
        let mut config = Config::builtin();
        config.default_cast = "deepseek".to_string();
        let usable = vec![("deepseek".to_string(), CastUsability::Ready)];
        let mut allowed = vec![PathBuf::from("/home/amy/src/some-project")];
        for n in 0..40 {
            allowed.push(PathBuf::from(format!(
                "/home/amy/src/wt/some-project-feature-branch-{n:02}"
            )));
            let text = kaibo_instructions_with_scope(
                &config,
                &allowed,
                Some(Path::new("/home/amy/src/some-project")),
                true,
                CastUsability::Ready,
                &usable,
            );
            let head = &text[..text.find("## Casts").expect("has a roster")];
            let len = head.chars().count();
            assert!(
                len <= 2048,
                "with {} allowed trees the lead and Scope are {len} chars, over the \
                 2048-char cut:\n{head}",
                allowed.len()
            );
            assert!(
                head.contains("More without spending a turn"),
                "Scope must keep its closing pointers:\n{head}"
            );
        }
    }

    /// Past [`ALLOWED_TREE_MAX_LINES`] Scope says how many trees it left out and where
    /// the full list is, so a capped list never reads as the whole boundary.
    #[test]
    fn a_capped_allowed_tree_list_names_the_count_left_out() {
        let config = Config::builtin();
        let allowed: Vec<PathBuf> = (0..10)
            .map(|n| PathBuf::from(format!("/srv/tree-{n:02}")))
            .collect();
        let text = kaibo_instructions_with_scope(
            &config,
            &allowed,
            None,
            false,
            CastUsability::Ready,
            &[],
        );
        let trees: Vec<&str> = text.lines().filter(|l| l.starts_with("  - ")).collect();
        assert_eq!(trees.len(), ALLOWED_TREE_MAX_LINES + 1, "{text}");
        assert_eq!(
            trees[ALLOWED_TREE_MAX_LINES],
            format!("  - +{} more: `kaibo://config`", 10 - ALLOWED_TREE_MAX_LINES),
            "{text}"
        );
    }

    /// Past [`ROSTER_MAX_LINES`] the roster says how many it left out and where they
    /// are, so a capped list never reads as the whole set. The default leads even when
    /// its name sorts last.
    #[test]
    fn a_capped_roster_names_the_count_left_out_and_leads_with_the_default() {
        let mut config = Config::builtin();
        let mut usable = Vec::new();
        for n in 0..12 {
            let name = format!("zz-{n:02}");
            config.casts.insert(
                name.clone(),
                Cast {
                    name: name.clone(),
                    slots: std::collections::BTreeMap::from([(
                        ModelRole::Synth,
                        ModelSlot::bare("deepseek", "deepseek-flash"),
                    )]),
                },
            );
            usable.push((name, CastUsability::Ready));
        }
        config.default_cast = "zz-11".to_string();
        let text = kaibo_instructions_with_scope(
            &config,
            &[PathBuf::from("/tmp")],
            None,
            false,
            CastUsability::Ready,
            &usable,
        );
        let roster: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("## Casts"))
            .filter(|l| l.starts_with("- "))
            .collect();
        assert_eq!(
            roster.len(),
            ROSTER_MAX_LINES + 1,
            "{ROSTER_MAX_LINES} cast lines plus the remainder line:\n{text}"
        );
        assert!(
            roster[0].starts_with("- `zz-11` (default)"),
            "the default leads the roster:\n{text}"
        );
        let left_out = 12 - ROSTER_MAX_LINES;
        assert_eq!(
            roster[ROSTER_MAX_LINES],
            format!("- +{left_out} more: `kaibo://config`"),
            "the last line counts the casts left out:\n{text}"
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
