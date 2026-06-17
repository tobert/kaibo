//! Follow git worktrees without running git.
//!
//! A `consult`/`run_kaish` `path` may point at a *linked* worktree that sits
//! outside the configured allowed set even though it belongs to the same repo as
//! an allowed tree (you check a branch out in a sibling dir, then ask kaibo about
//! it). Rejecting that is toil; `--allow-path` per sibling is more toil; rooting at
//! the parent (`~/src`) is strictly too broad. So when a path misses the static
//! allowed set we admit it iff it is a worktree of an *already-allowed* repo.
//!
//! We do this WITHOUT the `git` binary — `subprocess`/`git` are compiled out of the
//! sandbox (see `sandbox.rs`), and re-introducing them would breach the read-only
//! invariant. We don't need them: git's worktree links are plain text files, exactly
//! what kaibo's read-only product reads. The layout we walk:
//!
//! - A linked worktree's root holds a `.git` *file* (not dir):
//!   `gitdir: <common>/worktrees/<name>`.
//! - The repo's common git dir holds `worktrees/<name>/gitdir` (an absolute path
//!   *back* to that worktree's `.git` file) and `worktrees/<name>/commondir`
//!   (a relative path to the common dir).
//! - The main worktree's `.git` *is* the common dir.
//!
//! **Trust flows outward from the allowed repo, never inward from the candidate.**
//! We resolve the *allowed* tree's common dir (trusted: the operator allowed it),
//! enumerate the worktrees that common dir itself vouches for, and admit a candidate
//! only if it falls inside one. We never read the candidate's own `.git` to decide —
//! that file is attacker-controllable, so a one-way "candidate points into us" pull
//! would be spoofable (a hostile dir forging `gitdir:` to smuggle itself in). Letting
//! only the trusted side vouch is exactly what git itself does, and it makes the
//! spoof structurally impossible here: the candidate gets no say.

use std::path::{Path, PathBuf};

/// Resolve the canonicalized git *common* dir for `start` by reading git's link
/// files only. Walks up to the nearest ancestor holding a `.git` entry, then:
/// a `.git` directory is itself the common dir (main worktree); a `.git` *file*
/// points at `<common>/worktrees/<name>`, whose `commondir` resolves to the common
/// dir (linked worktree). `None` when `start` isn't inside a resolvable working tree.
pub fn common_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            // Main worktree (or a plain repo): `.git` is the common dir.
            return std::fs::canonicalize(&dotgit).ok();
        }
        if dotgit.is_file() {
            // Linked worktree: `.git` file → the per-worktree git dir → its commondir.
            let gitdir = read_gitdir_pointer(&dotgit)?;
            return resolve_commondir(&gitdir);
        }
        dir = dir.parent()?;
    }
}

/// The canonicalized working-tree roots a common git dir vouches for: the main
/// worktree (the parent of a `.git` common dir) plus every linked worktree
/// registered under `<common>/worktrees/<name>/gitdir`. These are the *only* paths
/// the worktree-follow feature will admit beyond the static allowed set.
pub fn vouched_worktrees(common_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // The main worktree is the parent of a `.git`-named common dir. A bare repo's
    // common dir isn't named `.git` and has no working tree — skip it then.
    if common_dir.file_name().and_then(|n| n.to_str()) == Some(".git") {
        if let Some(main) = common_dir.parent() {
            if let Ok(canon) = std::fs::canonicalize(main) {
                out.push(canon);
            }
        }
    }

    // Linked worktrees: each `worktrees/<name>/gitdir` holds the absolute path back
    // to that worktree's `.git` file; its parent is the worktree root. This is the
    // vouch — the trusted common dir naming where each of its worktrees lives.
    if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
        for entry in entries.flatten() {
            let pointer = entry.path().join("gitdir");
            let Some(wt_dotgit) = read_gitdir_pointer(&pointer) else {
                continue;
            };
            let Some(wt_root) = wt_dotgit.parent() else {
                continue;
            };
            if let Ok(canon) = std::fs::canonicalize(wt_root) {
                out.push(canon);
            }
        }
    }

    out
}

/// Read a git pointer file and return the absolute path it names, canonicalization
/// deferred to the caller (the target may be a `.git` file whose parent we want).
/// Handles both forms git writes: a worktree-root `.git` file (`gitdir: <path>`) and
/// a `worktrees/<name>/gitdir` file (a bare `<path>`). Relative targets resolve
/// against the pointer file's directory, as git does.
fn read_gitdir_pointer(pointer: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(pointer).ok()?;
    let raw = text.trim();
    let raw = raw.strip_prefix("gitdir:").map(str::trim).unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    let target = Path::new(raw);
    if target.is_absolute() {
        Some(target.to_path_buf())
    } else {
        // Relative to the directory holding the pointer file.
        pointer.parent().map(|d| d.join(target))
    }
}

/// Given a per-worktree git dir (`<common>/worktrees/<name>`), resolve the common
/// dir via its `commondir` file (a path relative to the git dir). When there's no
/// `commondir`, the git dir is itself the common dir. Canonicalized.
fn resolve_commondir(gitdir: &Path) -> Option<PathBuf> {
    let commondir_file = gitdir.join("commondir");
    match std::fs::read_to_string(&commondir_file) {
        Ok(text) => {
            let rel = text.trim();
            if rel.is_empty() {
                return std::fs::canonicalize(gitdir).ok();
            }
            let target = Path::new(rel);
            let joined = if target.is_absolute() {
                target.to_path_buf()
            } else {
                gitdir.join(target)
            };
            std::fs::canonicalize(joined).ok()
        }
        Err(_) => std::fs::canonicalize(gitdir).ok(),
    }
}
