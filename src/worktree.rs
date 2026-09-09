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
//! **Both ends of the link must name each other, and neither end is trusted alone.**
//! We resolve the *allowed* tree's common dir, enumerate the worktrees that common dir
//! vouches for, and admit a candidate only if it falls inside one. We never read the
//! candidate's own `.git` to decide — that file is attacker-controllable, so a one-way
//! "candidate points into us" pull would be spoofable (a hostile dir forging `gitdir:`
//! to smuggle itself in). And we do not take the allowed tree's own `.git` at its word
//! either: it is content kaibo did not author, so the common dir it names must name the
//! allowed tree back before any of its worktrees count, and each worktree it registers
//! must name that registration back before it counts. An earlier reading trusted that
//! side because "the operator allowed it" — but allowing kaibo to *read* a directory is
//! not the same grant as letting that directory *name other directories*. The everyday
//! case makes that sharp: a main worktree's common dir is `<tree>/.git`, *inside* the
//! allowed tree, so in any repository cloned to review every registration file under it
//! is attacker-authored.
//!
//! **The tree we were handed is the only tree that vouches.** [`common_git_dir`] reads
//! `<tree>/.git` and no ancestor of it, so a tree that merely sits inside a repository
//! reaches nothing: the enclosing repo's main worktree is by definition a *parent* of
//! the allowed tree, and vouching for it would widen the boundary upward to every
//! sibling the operator never named. Following worktrees reaches sideways, never up.
//! [`worktrees_reachable_from`] is where both rules live; containment calls nothing else.

use std::path::{Path, PathBuf};

/// Resolve the canonicalized git *common* dir for a worktree root, by reading git's
/// link files only. `worktree_root` must hold the `.git` entry itself: a `.git`
/// directory *is* the common dir (main worktree); a `.git` *file* points at
/// `<common>/worktrees/<name>`, whose `commondir` resolves to the common dir (linked
/// worktree). `None` for anything else — including a subdirectory of a repo.
///
/// **No ancestor walk, and that is the whole safety property.** Resolving a repo
/// *above* the given tree inverts the trust direction this module rests on: the
/// operator allowed one directory, and the enclosing repo's main worktree is by
/// definition a *parent* of it, so vouching for that repo hands out every sibling the
/// operator never named. A root under a home with dotfiles in git would have admitted
/// the entire home directory. The tree kaibo was given is the only tree that gets to
/// vouch, so it has to be the one holding `.git`.
pub fn common_git_dir(worktree_root: &Path) -> Option<PathBuf> {
    let dotgit = worktree_root.join(".git");
    if dotgit.is_dir() {
        // Main worktree (or a plain repo): `.git` is the common dir.
        return std::fs::canonicalize(&dotgit).ok();
    }
    if dotgit.is_file() {
        // Linked worktree: `.git` file → the per-worktree git dir → its commondir.
        let gitdir = read_gitdir_pointer(&dotgit)?;
        return resolve_commondir(&gitdir);
    }
    None
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

    // Linked worktrees: each `worktrees/<name>/gitdir` holds the absolute path back to
    // that worktree's `.git` file; its parent is the worktree root. Git writes the pair
    // — the registration names the worktree, and the worktree's own `.git` file names
    // the registration — and BOTH halves have to be present for the vouch to count.
    //
    // A one-way read of the registration is a read escape. When the allowed tree is a
    // main worktree its common dir is `<tree>/.git`, a directory *inside* the tree, so
    // in any repository kaibo did not author — one cloned to review, the everyday case
    // — the registration files are written by whoever wrote the repo. Naming
    // `/etc/.git` in a `gitdir` file would otherwise make `/etc` a vouched worktree
    // root: one file, and any directory on the host that is not an ancestor of the tree
    // becomes readable. Requiring the back-link kills it, because the named directory
    // has to point back and an attacker cannot write inside the directory they are
    // trying to reach.
    if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
        for entry in entries.flatten() {
            let registration = entry.path();
            let Some(wt_dotgit) = read_gitdir_pointer(&registration.join("gitdir")) else {
                continue;
            };
            let Some(wt_root) = wt_dotgit.parent() else {
                continue;
            };
            // The other half: `<wt>/.git` must name this exact registration back.
            let Some(back) = read_gitdir_pointer(&wt_dotgit) else {
                continue;
            };
            let (Ok(back), Ok(registration)) = (
                std::fs::canonicalize(&back),
                std::fs::canonicalize(&registration),
            ) else {
                continue;
            };
            if back != registration {
                continue;
            }
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

/// The working trees an allowed tree may reach beyond itself: the worktrees its own
/// common git dir vouches for, **and only when that common dir vouches for the allowed
/// tree back**. Empty when `tree` is not a worktree root, when the handshake fails, or
/// when there is nothing extra to reach. This is the one entry point containment uses;
/// [`common_git_dir`] and [`vouched_worktrees`] are its halves.
///
/// **The handshake is the trust, in both directions.** `tree/.git` is content kaibo did
/// not author — a repository cloned to review carries whatever its author put there —
/// so a `.git` *file* naming an arbitrary git dir can aim `commondir` at any directory
/// on the host, and vouching for that directory's worktrees would hand out a tree the
/// operator never named. Allowing kaibo to *read* a directory is not the same grant as
/// letting that directory *name other directories*. So the common dir must list `tree`
/// itself among its worktree roots: a forged pointer reaches a common dir that has never
/// heard of `tree`, and the reach collapses to nothing. This is the same two-way check
/// the candidate side already got, applied to the side the reach starts from.
///
/// That handshake alone is not enough, which is why [`vouched_worktrees`] carries its
/// own: when `tree` is a main worktree the common dir lives *inside* it, so `tree` is a
/// vouched root by construction and the handshake here passes for free, saying nothing
/// about the entries beside it. Each registered worktree earns its place separately, by
/// naming its registration back.
///
/// A vouched tree that strictly *contains* `tree` is dropped as well — a genuine but
/// unusual layout (`git worktree add` inside the main worktree) would otherwise widen
/// the boundary upward to the parent, which is the one thing following worktrees
/// promises not to do. Reach that tree with `--allow-path`.
pub fn worktrees_reachable_from(tree: &Path) -> Vec<PathBuf> {
    let Some(common) = common_git_dir(tree) else {
        return Vec::new();
    };
    let vouched = vouched_worktrees(&common);
    // The handshake: this common dir must name `tree` as one of its own worktree roots.
    if !vouched.iter().any(|wt| wt == tree) {
        return Vec::new();
    }
    vouched
        .into_iter()
        .filter(|wt| wt == tree || !tree.starts_with(wt))
        .collect()
}

#[cfg(test)]
mod tests {
    //! Pure filesystem tests: hand-craft git's link-file layout with no `git`
    //! binary at all (unlike `tests/containment.rs`'s worktree-follow battery,
    //! which shells out to real `git worktree add` and self-skips when git isn't
    //! on PATH). These run unconditionally, on every host including a git-less
    //! CI runner, so the core parsing/trust logic of this module is proven even
    //! where the integration battery silently no-ops. They exercise exactly the
    //! layout described in the module doc-comment, not `containing_tree`'s
    //! allowed-set logic (that's `tests/containment.rs`'s job).

    use super::*;
    use tempfile::tempdir;

    /// Write git's real worktree-registration layout under `base`:
    /// `repo/.git/` (a directory: the common dir) with one linked worktree
    /// `name` registered at `wt`, wired exactly as git itself writes it —
    /// `worktrees/<name>/gitdir` back-linking to `<wt>/.git`, and
    /// `worktrees/<name>/commondir` as the relative `../..` git always writes
    /// (two components: `worktrees/<name>`, popped to reach `.git`). Returns
    /// `(repo, wt)`, both created but not canonicalized.
    fn write_registered_worktree(base: &Path, name: &str) -> (PathBuf, PathBuf) {
        let repo = base.join("repo");
        let dotgit = repo.join(".git");
        let wt_gitdir_folder = dotgit.join("worktrees").join(name);
        std::fs::create_dir_all(&wt_gitdir_folder).unwrap();
        let wt = base.join(name);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir_folder.display()),
        )
        .unwrap();
        std::fs::write(
            wt_gitdir_folder.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        std::fs::write(wt_gitdir_folder.join("commondir"), "../..\n").unwrap();
        (repo, wt)
    }

    /// The headline shape: a linked worktree's `.git` file resolves, via its
    /// registered git dir's `commondir`, to the exact same common dir the main
    /// worktree's own `.git` directory *is* — proving `common_git_dir` is
    /// symmetric regardless of which side you start walking from (the property
    /// `sibling_reachable_when_rooted_at_linked_worktree` exercises end-to-end
    /// in `tests/containment.rs`).
    #[test]
    fn common_git_dir_agrees_from_either_side() {
        let base = tempdir().unwrap();
        let (repo, wt) = write_registered_worktree(base.path(), "feature");

        let from_main = common_git_dir(&repo).expect("main worktree resolves a common dir");
        let from_linked = common_git_dir(&wt).expect("linked worktree resolves a common dir");
        assert_eq!(
            from_main, from_linked,
            "both sides of one repo must agree on the common dir"
        );
        assert_eq!(
            from_main,
            std::fs::canonicalize(repo.join(".git")).unwrap(),
            "the common dir must be the main worktree's own `.git`"
        );
    }

    /// `worktrees_reachable_from` hands back only trees whose registration is mutual.
    /// The allowed tree here is a main worktree, so its common dir sits *inside* it and
    /// every byte of it belongs to whoever wrote the repo — the shape of any repository
    /// cloned to review. One `worktrees/<name>/gitdir` naming an outside directory used
    /// to vouch for it; now the named directory has to point back, which it cannot,
    /// because writing inside it is the very access being sought.
    ///
    /// The `tree`-side handshake cannot catch this on its own: the main worktree entry
    /// *is* `tree`, so it passes for free and says nothing about its neighbours.
    #[test]
    fn a_registration_alone_does_not_vouch_for_the_directory_it_names() {
        let base = tempdir().unwrap();
        let project = base.path().join("project");
        let dotgit = project.join(".git");
        std::fs::create_dir_all(dotgit.join("worktrees").join("evil")).unwrap();
        let victim = base.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(
            dotgit.join("worktrees").join("evil").join("gitdir"),
            format!("{}\n", victim.join(".git").display()),
        )
        .unwrap();

        let canon_project = std::fs::canonicalize(&project).unwrap();
        let reachable = worktrees_reachable_from(&canon_project);
        assert_eq!(
            reachable,
            vec![canon_project.clone()],
            "only the allowed tree itself may be reachable, got {reachable:?}"
        );

        // Positive control: a *mutually* registered worktree does come back, so the
        // assertion above is the back-link check firing rather than the whole feature
        // being dead.
        let (repo, wt) = write_registered_worktree(base.path(), "feature");
        let canon_repo = std::fs::canonicalize(&repo).unwrap();
        let canon_wt = std::fs::canonicalize(&wt).unwrap();
        assert!(
            worktrees_reachable_from(&canon_repo).contains(&canon_wt),
            "a mutually registered worktree must still be reachable"
        );
    }

    /// A subdirectory of a repo resolves *nothing*. The helper reads the tree it was
    /// handed and no ancestor of it — so an allowed tree that merely sits inside
    /// someone's repository can never make that repository vouch for its own siblings.
    /// The walk this replaced admitted every sibling of the enclosing repo, and a root
    /// under a home with dotfiles in git admitted the whole home directory
    /// (`tests/containment.rs::a_repo_above_the_allowed_tree_does_not_widen_the_boundary`
    /// is the end-to-end half).
    #[test]
    fn a_subdirectory_of_a_repo_resolves_no_common_dir() {
        let base = tempdir().unwrap();
        let (repo, _wt) = write_registered_worktree(base.path(), "feature");
        let sub = repo.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        // Positive control: the repo root itself still resolves, so a `None` below
        // means the walk stopped, not that the fixture is unreadable.
        assert!(
            common_git_dir(&repo).is_some(),
            "the repo root must still resolve its own common dir"
        );
        assert_eq!(
            common_git_dir(&sub),
            None,
            "a subdirectory holds no `.git`, so it vouches for nothing"
        );
        assert_eq!(
            common_git_dir(base.path()),
            None,
            "the directory *containing* the repo vouches for nothing either"
        );
    }

    /// `vouched_worktrees` on that common dir names exactly the main worktree
    /// plus the one registered linked worktree — the *only* paths the follow
    /// feature will ever admit for this repo.
    #[test]
    fn vouched_worktrees_lists_main_plus_each_registered_linked_worktree() {
        let base = tempdir().unwrap();
        let (repo, wt) = write_registered_worktree(base.path(), "feature");
        let common = common_git_dir(&repo).unwrap();

        let vouched = vouched_worktrees(&common);
        assert!(
            vouched.contains(&std::fs::canonicalize(&repo).unwrap()),
            "must vouch for the main worktree: {vouched:?}"
        );
        assert!(
            vouched.contains(&std::fs::canonicalize(&wt).unwrap()),
            "must vouch for the registered linked worktree: {vouched:?}"
        );
        assert_eq!(
            vouched.len(),
            2,
            "must name exactly main + the one registered worktree, nothing else: {vouched:?}"
        );
    }

    /// A `worktrees/<name>/` entry with no `gitdir` back-link file (a partial or
    /// corrupt registration) must be skipped rather than panicking or fabricating
    /// a path — `vouched_worktrees` reads options throughout precisely so a
    /// malformed entry silently doesn't vouch for anything, instead of crashing
    /// the whole containment check for every other call.
    #[test]
    fn vouched_worktrees_skips_a_registration_missing_its_gitdir_file() {
        let base = tempdir().unwrap();
        let (repo, _wt) = write_registered_worktree(base.path(), "feature");
        // A second, malformed registration: the directory exists but never got
        // its `gitdir` back-link written (e.g. an interrupted `git worktree add`).
        std::fs::create_dir_all(repo.join(".git").join("worktrees").join("half-done")).unwrap();
        let common = common_git_dir(&repo).unwrap();

        let vouched = vouched_worktrees(&common);
        assert_eq!(
            vouched.len(),
            2,
            "the malformed entry must contribute nothing, not a bogus third path: {vouched:?}"
        );
    }

    /// The one-way trust property, proven at the primitive level with no `git`
    /// binary and no resolver involved: `vouched_worktrees` takes only the
    /// *trusted* common dir as input, never the candidate path, so a foreign
    /// directory's own forged `.git` file — even one pointing at a real
    /// registered worktree's git dir, exactly as
    /// `tests/containment.rs::spoofed_dotgit_pointing_into_allowed_repo_is_rejected`
    /// builds end-to-end — cannot possibly appear in the vouched set: the
    /// function never reads it. This is what makes the spoof structurally
    /// impossible rather than merely unlikely.
    #[test]
    fn vouched_worktrees_cannot_be_steered_by_a_foreign_dotgit_pointer() {
        let base = tempdir().unwrap();
        let (repo, wt) = write_registered_worktree(base.path(), "feature");
        let common = common_git_dir(&repo).unwrap();

        // A foreign directory, never registered under `repo/.git/worktrees/`, whose
        // own `.git` file points at the SAME real git dir `wt` is registered under —
        // impersonating it from the candidate side.
        let spoof = base.path().join("spoof");
        std::fs::create_dir_all(&spoof).unwrap();
        let real_gitdir_folder = repo.join(".git").join("worktrees").join("feature");
        std::fs::write(
            spoof.join(".git"),
            format!("gitdir: {}\n", real_gitdir_folder.display()),
        )
        .unwrap();

        let vouched = vouched_worktrees(&common);
        assert!(
            vouched.contains(&std::fs::canonicalize(&wt).unwrap()),
            "the genuine registered worktree must still be vouched for: {vouched:?}"
        );
        assert!(
            !vouched.contains(&std::fs::canonicalize(&spoof).unwrap()),
            "a foreign dir's own forged .git must never appear in the vouched set: {vouched:?}"
        );
        assert_eq!(
            vouched.len(),
            2,
            "spoofing must add nothing to the vouched set: {vouched:?}"
        );
    }
}
