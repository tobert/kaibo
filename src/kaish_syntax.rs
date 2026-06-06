//! One source of truth for how kaibo describes its read-only kaish shell.
//!
//! The same idioms used to live three times — the explorer preamble, the report
//! preamble, and the `run_kaish` tool definition — and drifted. They now all
//! compose [`KAISH_SYNTAX_CORE`]: the compact, model-facing block that foregrounds
//! line-number-aware browsing (so citations come out as accurate `file:line`s),
//! lists the builtins, and states the exit-code contract. [`kaish_syntax_resource`]
//! is the verbose superset exposed at `kaibo://kaish-syntax` for callers (and hosts)
//! that fetch resources.
//!
//! `concat!` can't splice a `const` ident, so the composing pieces are
//! `fn -> String` (cheap — built once per phase) rather than more consts.

/// The compact, model-facing cheatsheet. Every internal preamble and the
/// `run_kaish` tool definition embed this verbatim, so there is exactly one place
/// to edit the idioms and the exit-code contract.
pub const KAISH_SYNTAX_CORE: &str = "\
You read a project through `run_kaish`, which runs a kaish (sh-like) script over a \
READ-ONLY filesystem and returns its exit code, stdout, and stderr. Browse code with \
line numbers so every citation is exact: `cat -n FILE`, `rg -n PATTERN`, and numbered \
spans like `cat -n FILE | sed -n '40,80p'`. Compose builtins with pipes and `$(...)` — \
ls, cat, head, tail, grep, rg, find, jq, awk, cut, sed, sort, uniq, wc, diff, tree, and \
more — e.g. `rg -l TODO src | head` or `cat -n Cargo.toml | sed -n '1,20p'`. Each call \
starts at the project root; there is no persistent cwd. The sandbox is read-only and \
offline, so work within it and just read: writes, `git`, `touch`, and external commands \
are refused — exit 126 means blocked by the sandbox, 124 means a script was killed for \
running past its time budget, 127 means command-not-found, and any other non-zero means \
the script itself failed.";

/// The `run_kaish` (rig) tool description shown to the internal models. It *is*
/// the shared core — same idioms, same exit-code contract, no drift.
pub(crate) fn run_kaish_tool_description() -> String {
    KAISH_SYNTAX_CORE.to_string()
}

/// The verbose reference served at `kaibo://kaish-syntax`. A superset of the core:
/// the same ground truth, expanded for a human or a host that fetches it.
pub fn kaish_syntax_resource() -> String {
    format!(
        "# kaibo — read-only kaish shell\n\n\
         {KAISH_SYNTAX_CORE}\n\n\
         ## Browsing for exact citations\n\
         Lead with line numbers so the answer can cite `file:line`:\n\
         - `cat -n FILE` — file with line numbers\n\
         - `rg -n PATTERN [PATH]` — matches with line numbers\n\
         - `cat -n FILE | sed -n '40,80p'` — a numbered span\n\
         - `rg -l PATTERN src` — just the file names that match\n\n\
         ## Builtins and pipelines\n\
         ls, cat, head, tail, grep, rg, find, jq, awk, cut, sed, sort, uniq, wc, diff, \
         tree, and more. Compose them with pipes and `$(...)`, e.g. \
         `cat Cargo.toml | grep '^name'`.\n\n\
         ## Read-only boundary\n\
         The project is mounted read-only and external commands are off, by \
         construction. Writes, `git`, `touch`, `spawn`/`exec`, and any external \
         command are refused. This is the product: read freely, expect no write to \
         land.\n\n\
         ## Exit codes\n\
         - `0` — success\n\
         - `124` — killed for exceeding the per-exec time budget\n\
         - `126` — blocked by the read-only sandbox (collides with POSIX \
         \"not executable\" — read the message to be sure)\n\
         - `127` — command not found\n\
         - other non-zero — the script itself failed\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_is_a_superset_of_the_core() {
        assert!(
            kaish_syntax_resource().contains(KAISH_SYNTAX_CORE),
            "the verbose resource must embed the shared core verbatim"
        );
    }

    #[test]
    fn the_tool_description_is_the_core() {
        assert!(run_kaish_tool_description().contains(KAISH_SYNTAX_CORE));
    }

    #[test]
    fn core_states_the_exit_code_contract_and_line_browsing() {
        // The two things the synth preamble rewards (exact file:line) and the two
        // codes an automated caller will misread without help.
        for needle in ["cat -n", "rg -n", "126", "124"] {
            assert!(
                KAISH_SYNTAX_CORE.contains(needle),
                "core must mention {needle:?}"
            );
        }
    }
}
