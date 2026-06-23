//! Operator "house rules" — project and user guidance spliced into every
//! consultation tool's preamble so kaibo's models inherit the calling agent's
//! conventions (an `AGENTS.md`, a shared `~/.claude/CLAUDE.md`, whatever the
//! operator named). Vendor-neutral: no filename is hardcoded in the product —
//! `project_files` defaults to `["AGENTS.md"]` (the emerging cross-tool
//! convention) and everything else is config (`config.rs`, `[context]`).
//!
//! Two source lists with deliberately different failure semantics, because they
//! mean different things:
//!
//! - **`project_files`** are root-relative and **read-if-present**: an absent
//!   `AGENTS.md` is the normal case, not an error. Each is joined to the resolved
//!   project root and canonicalize-checked to stay *within* it — a configured
//!   `../escape` (or a symlink out) is refused, so the same containment that
//!   bounds the read-only shell also bounds what gets injected.
//! - **`user_files`** are absolute (`$VAR`/`~` already expanded at config merge) and
//!   **read-required**: the operator named this file deliberately, so a missing
//!   one is a loud error, not a silent skip. These live *outside* the sandbox's
//!   allowed set on purpose — they're read here in trusted Rust at the server
//!   handler (the same trust level as `config.toml` itself) and only their
//!   *contents* reach the model, never the path, so the shell's read scope is
//!   not widened. Crashing on a missing declared file beats silently dropping
//!   the guidance the operator was counting on.
//!
//! The assembled block is framed (in `consult.rs`) as standing background, not as
//! the question — see [`ContextConfig::assemble`].

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Resolved `[context]` configuration: which files to splice into preambles.
/// Built by `config.rs::merge_context` from the on-disk `[context]` table (with
/// `project_files` defaulting to `["AGENTS.md"]`); `user_files` arrive already
/// `$VAR`/`~`-expanded so `assemble` does pure filesystem work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextConfig {
    /// Root-relative files read if present (absent is normal). Default
    /// `["AGENTS.md"]`. Each must canonicalize to at-or-under the project root.
    pub project_files: Vec<String>,
    /// Absolute files (`$VAR`/`~` already expanded) read unconditionally — a missing
    /// one is an error, since the operator declared it. Default empty.
    pub user_files: Vec<PathBuf>,
}

impl ContextConfig {
    /// Read the configured files against `root` and concatenate them into one
    /// preamble block, or `None` when nothing resolves (no files configured, or
    /// only absent project files).
    ///
    /// Project files are read-if-present and containment-checked; user files are
    /// read-required. Each section is headed with its provenance so the model
    /// knows what it's reading (and so two files can't silently blur together).
    pub fn assemble(&self, root: &Path) -> Result<Option<String>> {
        let mut sections: Vec<String> = Vec::new();

        if !self.project_files.is_empty() {
            // Canonicalize the root once so the per-file containment check has a
            // resolved tree to compare against (symlinks/.. settled), matching
            // `server.rs::resolve_root`'s discipline.
            let canon_root = std::fs::canonicalize(root).with_context(|| {
                format!("resolving project root {} for context", root.display())
            })?;
            for name in &self.project_files {
                let joined = canon_root.join(name);
                // Absent is the normal case for project files — skip, no error.
                if !joined.exists() {
                    continue;
                }
                let canon = std::fs::canonicalize(&joined).with_context(|| {
                    format!("resolving project context file {}", joined.display())
                })?;
                // Containment teeth: a configured `../` or an out-of-tree symlink
                // would otherwise inject arbitrary file contents into the preamble.
                if !canon.starts_with(&canon_root) {
                    bail!(
                        "project context file {name:?} resolves to {}, outside the project \
                         root {} — [context] project_files must stay within the project; \
                         use [context] user_files for guidance that lives elsewhere",
                        canon.display(),
                        canon_root.display()
                    );
                }
                let body = std::fs::read_to_string(&canon)
                    .with_context(|| format!("reading project context file {}", canon.display()))?;
                sections.push(section(&format!("project: {name}"), &body));
            }
        }

        for path in &self.user_files {
            // Declared → required. A missing user file is a loud error: the
            // operator named it on purpose, so silently dropping it would ship an
            // answer missing the guidance they were counting on.
            let body = std::fs::read_to_string(path).with_context(|| {
                format!(
                    "reading user context file {} (configured in [context] user_files — a \
                     declared file must exist; remove it from config if that's intended)",
                    path.display()
                )
            })?;
            sections.push(section(&format!("user: {}", path.display()), &body));
        }

        if sections.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sections.join("\n\n")))
        }
    }
}

/// One provenance-headed section. Trailing whitespace trimmed so concatenation
/// stays tidy; the header names where the bytes came from.
fn section(provenance: &str, body: &str) -> String {
    format!("### {provenance}\n\n{}", body.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Nothing configured → nothing injected. A bare server adds no preamble bulk.
    #[test]
    fn empty_config_assembles_to_none() {
        let dir = tempdir().unwrap();
        let ctx = ContextConfig::default();
        assert_eq!(ctx.assemble(dir.path()).unwrap(), None);
    }

    /// A present project file is read and headed with its provenance.
    #[test]
    fn present_project_file_is_included_with_header() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Use tabs, not spaces.\n").unwrap();
        let ctx = ContextConfig {
            project_files: vec!["AGENTS.md".into()],
            user_files: vec![],
        };
        let out = ctx.assemble(dir.path()).unwrap().expect("some context");
        assert!(
            out.contains("### project: AGENTS.md"),
            "header missing: {out}"
        );
        assert!(out.contains("Use tabs, not spaces."), "body missing: {out}");
    }

    /// An absent project file is the normal case — skipped, not an error.
    #[test]
    fn absent_project_file_is_skipped_not_an_error() {
        let dir = tempdir().unwrap();
        let ctx = ContextConfig {
            project_files: vec!["AGENTS.md".into()],
            user_files: vec![],
        };
        // No AGENTS.md on disk: assembles cleanly to None.
        assert_eq!(ctx.assemble(dir.path()).unwrap(), None);
    }

    /// A declared user file that's missing is a loud error — the operator named
    /// it, so silently dropping their guidance is exactly the failure to avoid.
    #[test]
    fn missing_user_file_is_a_loud_error() {
        let dir = tempdir().unwrap();
        let ctx = ContextConfig {
            project_files: vec![],
            user_files: vec![dir.path().join("nope-not-here.md")],
        };
        let err = ctx.assemble(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("user context file"), "wrong error: {msg}");
    }

    /// A present user file is read and headed as user provenance.
    #[test]
    fn present_user_file_is_included() {
        let dir = tempdir().unwrap();
        let user = dir.path().join("CLAUDE.md");
        fs::write(&user, "We practice TDD.\n").unwrap();
        let ctx = ContextConfig {
            project_files: vec![],
            user_files: vec![user.clone()],
        };
        let out = ctx.assemble(dir.path()).unwrap().expect("some context");
        assert!(out.contains("### user:"), "header missing: {out}");
        assert!(out.contains("We practice TDD."), "body missing: {out}");
    }

    /// A project file that escapes the root (via `..`) is refused — the same
    /// containment that bounds the read-only shell bounds preamble injection.
    /// Failing-first with teeth: the bytes outside the root must never land in
    /// the preamble.
    #[test]
    fn project_file_escaping_root_is_refused() {
        let outer = tempdir().unwrap();
        // The secret lives a level above the project root.
        fs::write(outer.path().join("secret.md"), "exfiltrate me").unwrap();
        let root = outer.path().join("project");
        fs::create_dir(&root).unwrap();
        let ctx = ContextConfig {
            project_files: vec!["../secret.md".into()],
            user_files: vec![],
        };
        let err = ctx.assemble(&root).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("outside the project root"),
            "wrong error: {msg}"
        );
        assert!(!msg.contains("exfiltrate me"), "leaked the body: {msg}");
    }

    /// Project then user, in configured order, both present — sections concatenate
    /// with a blank line between, each under its own header.
    #[test]
    fn project_and_user_sections_concatenate_in_order() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "project rule").unwrap();
        let user = dir.path().join("user.md");
        fs::write(&user, "user rule").unwrap();
        let ctx = ContextConfig {
            project_files: vec!["AGENTS.md".into()],
            user_files: vec![user],
        };
        let out = ctx.assemble(dir.path()).unwrap().unwrap();
        let proj = out.find("project rule").unwrap();
        let usr = out.find("user rule").unwrap();
        assert!(proj < usr, "project should precede user: {out}");
    }
}
