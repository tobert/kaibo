//! Static repo orientation: a size-gated, computed-once file map spliced into the
//! exploring phases' preamble so a model starts *knowing* the project's files
//! instead of spending its first turns on `glob`/`ls`/`find` to discover the
//! layout. This is the structure-first lesson (Agentless/Aider) made free — no
//! model in the loop, computed server-side.
//!
//! It leans on kaish's own tools rather than reimplementing them: `glob -a --json
//! '**/*'` run through the kernel is the *same* ignore-aware enumeration the model's
//! shell would get (same VFS, same ignore config), so the map can never disagree
//! with what the explorer's own `glob`/`grep` sees — one source of truth. (`-a`
//! includes hidden config like `.github/`/`.cargo/`; the ignore filter still drops
//! `.git`/`target`.)
//!
//! Size-gated, with a graceful descent — orientation is an *enhancement* (the model
//! always has `glob`/`grep`/`explore′` regardless), so its absence must never be
//! fatal. At or under `full_list_max_files` the whole file list is injected. Above
//! it the flat list would be too big, so we fall back to a **directory map**: the
//! same files folded into a depth-limited tree of dir → file-count lines, which
//! gives the model the layout without the line budget of every path. If even that
//! map would exceed the line budget (a very large or very wide repo), orientation
//! degrades to a short note pointing the model at discovery-as-you-go — logged, not
//! silent. The call is never refused for being large; that was the old behavior and
//! it turned a missing nicety into a hard failure.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::sandbox::{KaishWorker, SandboxConfig};

/// Resolved `[orientation]` config: whether to inject the repo map, the file-count
/// ceiling that switches the full list to a directory map, and how deep that map
/// descends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrientationConfig {
    pub enabled: bool,
    /// At or under this many files, inject the complete file list. Above it, fall
    /// back to a directory map. Doubles as the line budget for that map: if the map
    /// would render more directory lines than this, orientation degrades to a note.
    pub full_list_max_files: usize,
    /// How many directory levels the fallback map descends before folding deeper
    /// files into the count of the deepest shown directory. Keeps a deep monorepo's
    /// map bounded.
    pub tree_max_depth: usize,
}

impl Default for OrientationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            full_list_max_files: 256,
            tree_max_depth: 4,
        }
    }
}

impl OrientationConfig {
    /// Build the orientation block for `root`, or `None` when disabled or the repo
    /// is empty. Never errors on size — a large repo gets a directory map, and a
    /// repo too large for even that gets a discover-as-you-go note (logged). The
    /// only hard errors are a failed kernel spawn or unparseable enumeration.
    pub async fn assemble(&self, root: &Path, sandbox: SandboxConfig) -> Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }
        let worker = KaishWorker::spawn_with(root, sandbox)
            .context("orientation: spawning the read-only kernel")?;
        let files = list_files(&worker).await?;
        if files.is_empty() {
            return Ok(None);
        }
        let n = files.len();
        if n <= self.full_list_max_files {
            return Ok(Some(render_full_list(&files)));
        }
        // Too many files for a flat list — fold into a directory map.
        let tree = DirNode::from_paths(&files);
        let dir_lines = tree.rendered_dir_count(1, self.tree_max_depth);
        if dir_lines > self.full_list_max_files {
            // Even the directory map exceeds the line budget — a very large or very
            // wide repo. Degrade to a note rather than dump (or refuse). Loud in the
            // log so the operator can see the map was skipped and why.
            tracing::warn!(
                files = n,
                directories = dir_lines,
                budget = self.full_list_max_files,
                "orientation: repo too large for a directory map; injecting a \
                 discover-as-you-go note instead"
            );
            return Ok(Some(render_too_large_note(n, dir_lines)));
        }
        Ok(Some(render_tree(&tree, n, self.tree_max_depth)))
    }
}

/// Enumerate the project's files via the kernel's own `glob` — the same ignore-aware
/// view the model's shell gets. `-a` includes hidden config files; `--json` gives a
/// parseable array (no stdout scraping). A glob failure degrades to "no files" (the
/// orientation is an enhancement; a repo we can't enumerate just doesn't get one),
/// except a real spawn/kernel error, which already bailed above.
async fn list_files(worker: &KaishWorker) -> Result<Vec<String>> {
    let out = worker
        .run("glob -a --json '**/*'")
        .await
        .context("orientation: running glob")?;
    if !out.ok() {
        // `glob` errors on zero matches (strict globs) — treat an un-enumerable or
        // empty project as "no map", not a crash. The exploring phase still works.
        return Ok(Vec::new());
    }
    let trimmed = out.stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let files: Vec<String> = serde_json::from_str(trimmed)
        .with_context(|| format!("orientation: parsing glob --json output: {trimmed:.200}"))?;
    Ok(files)
}

/// Render the complete file list into the injected block. The framing tells the
/// model the map is complete (so it skips discovery) and points it at the reads it
/// should do instead — the whole point is to convert "what's here?" turns into
/// direct reads.
fn render_full_list(files: &[String]) -> String {
    let mut s = String::with_capacity(64 + files.iter().map(|f| f.len() + 1).sum::<usize>());
    s.push_str(
        "PROJECT FILES. The project's complete file list (read-only; hidden config \
         included, build/VCS dirs excluded). You already have the whole layout here, \
         so go straight to reading the files the question touches with `cat -n FILE`, \
         and use `grep -rn` to find where something lives inside them.\n",
    );
    for f in files {
        s.push_str("  ");
        s.push_str(f);
        s.push('\n');
    }
    s
}

/// Render the depth-limited directory map for a repo too large to list flat. Each
/// line is a directory and the total file count under it; the framing tells the
/// model the names were traded for structure and how to recover them (`glob` a
/// directory, `grep -rn` to locate, `cat -n` to read).
fn render_tree(root: &DirNode, total_files: usize, max_depth: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "PROJECT STRUCTURE. This project has {total_files} files — too many to list \
         individually, so here is its directory map (read-only; build/VCS dirs \
         excluded). Each line is a directory and the number of files under it. Go \
         straight to the directories the question touches: `grep -rn PATTERN DIR/` to \
         find where something lives, then `cat -n DIR/FILE` to read it; `glob \
         'DIR/**/*'` lists a directory's files when you need exact names.\n"
    ));
    if root.direct_files > 0 {
        s.push_str(&format!(
            "  ./  {}\n",
            count_phrase(root.direct_files)
        ));
    }
    root.render_children("", 1, max_depth, &mut s);
    s
}

/// Render the discover-as-you-go note for a repo too large for even a directory map.
/// Positive, action-first framing: name the tools that find structure on demand
/// rather than dwelling on the absence of a map.
fn render_too_large_note(total_files: usize, directories: usize) -> String {
    format!(
        "PROJECT STRUCTURE. This project is very large ({total_files} files across \
         {directories}+ directories) — too big for a file or directory map. Discover \
         the layout as you go: `glob '**/*.rs'` (or another extension) to list files \
         of a kind, `grep -rln PATTERN` to find where something lives, then `cat -n \
         FILE` to read it.\n"
    )
}

/// "1 file" / "N files" — pluralized so the map reads naturally.
fn count_phrase(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// A node in the directory tree: files directly in this directory, and named child
/// directories. Built purely from the file path list — no second enumeration, so it
/// can never disagree with the full-list form.
#[derive(Default)]
struct DirNode {
    direct_files: usize,
    children: BTreeMap<String, DirNode>,
}

impl DirNode {
    /// Fold a list of `/`-separated relative file paths into a tree. The final
    /// component is the file (counted on its directory); the rest are directories.
    fn from_paths(files: &[String]) -> DirNode {
        let mut root = DirNode::default();
        for f in files {
            let comps: Vec<&str> = f.split('/').filter(|c| !c.is_empty()).collect();
            root.insert(&comps);
        }
        root
    }

    fn insert(&mut self, comps: &[&str]) {
        match comps {
            [] => {}
            [_file] => self.direct_files += 1,
            [dir, rest @ ..] => self
                .children
                .entry((*dir).to_string())
                .or_default()
                .insert(rest),
        }
    }

    /// Total files at or under this node.
    fn total_files(&self) -> usize {
        self.direct_files + self.children.values().map(DirNode::total_files).sum::<usize>()
    }

    /// How many directory lines `render_children` would emit at this depth/limit —
    /// the line-budget check, computed without building the string.
    fn rendered_dir_count(&self, depth: usize, max_depth: usize) -> usize {
        if depth > max_depth {
            return 0;
        }
        self.children
            .values()
            .map(|c| 1 + c.rendered_dir_count(depth + 1, max_depth))
            .sum()
    }

    /// Emit `prefix-qualified DIR/  N files` lines, descending until `max_depth`.
    /// Past the depth limit, deeper files stay folded into a directory's total
    /// count (which already includes them) — the structure is summarized, not lost.
    fn render_children(&self, prefix: &str, depth: usize, max_depth: usize, s: &mut String) {
        if depth > max_depth {
            return;
        }
        for (name, child) in &self.children {
            let path = format!("{prefix}{name}/");
            let indent = "  ".repeat(depth);
            s.push_str(&format!(
                "{indent}{path}  {}\n",
                count_phrase(child.total_files())
            ));
            child.render_children(&path, depth + 1, max_depth, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    /// A small repo gets its whole file list, framed so the model skips discovery.
    #[tokio::test]
    async fn lists_a_small_repo() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        write(dir.path(), "README.md", "# hi\n");
        let canon = std::fs::canonicalize(dir.path()).unwrap();

        let out = OrientationConfig::default()
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap()
            .expect("a non-empty repo yields a map");
        assert!(out.contains("PROJECT FILES"), "framed: {out}");
        assert!(out.contains("src/main.rs"), "lists the source: {out}");
        assert!(out.contains("README.md"), "lists the readme: {out}");
    }

    /// Over the file-count ceiling, the flat list gives way to a directory map —
    /// dir → file-count lines — instead of refusing the call. Orientation is an
    /// enhancement; a large repo must still get one.
    #[tokio::test]
    async fn summarizes_a_repo_over_the_limit_as_a_dir_map() {
        let dir = tempdir().unwrap();
        // 6 files across two directories, over a ceiling of 3.
        write(dir.path(), "src/a.rs", "x\n");
        write(dir.path(), "src/b.rs", "x\n");
        write(dir.path(), "src/c.rs", "x\n");
        write(dir.path(), "docs/one.md", "x\n");
        write(dir.path(), "docs/two.md", "x\n");
        write(dir.path(), "README.md", "x\n");
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = OrientationConfig {
            enabled: true,
            full_list_max_files: 3,
            tree_max_depth: 4,
        };
        let out = cfg
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap()
            .expect("a large repo still yields a map");
        assert!(out.contains("PROJECT STRUCTURE"), "framed as structure: {out}");
        assert!(out.contains("src/  3 files"), "src dir + count: {out}");
        assert!(out.contains("docs/  2 files"), "docs dir + count: {out}");
        // The flat names are traded for structure — no individual source file listed.
        assert!(!out.contains("a.rs"), "names are folded into counts: {out}");
        // The root file is reflected in the root line, singular-pluralized.
        assert!(out.contains("./  1 file"), "root file counted: {out}");
    }

    /// The directory map descends only `tree_max_depth` levels; files deeper than
    /// that stay folded into the deepest shown directory's total count, so a deep
    /// tree can't blow the budget.
    #[tokio::test]
    async fn dir_map_respects_max_depth() {
        let dir = tempdir().unwrap();
        for i in 0..5 {
            write(dir.path(), &format!("a/b/c/deep{i}.rs"), "x\n");
        }
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = OrientationConfig {
            enabled: true,
            full_list_max_files: 3,
            tree_max_depth: 2,
        };
        let out = cfg
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap()
            .expect("yields a map");
        assert!(out.contains("a/  5 files"), "depth-1 dir + total: {out}");
        assert!(out.contains("a/b/  5 files"), "depth-2 dir + total: {out}");
        assert!(!out.contains("a/b/c/"), "depth-3 dir folded away: {out}");
    }

    /// When even the directory map would exceed the line budget (more directories
    /// than `full_list_max_files`), orientation degrades to a discover-as-you-go
    /// note — never a refusal, never a dump.
    #[tokio::test]
    async fn very_wide_repo_degrades_to_a_note() {
        let dir = tempdir().unwrap();
        // 5 sibling directories, each one file — 5 dir lines, over a ceiling of 2.
        for i in 0..5 {
            write(dir.path(), &format!("d{i}/f.rs"), "x\n");
        }
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = OrientationConfig {
            enabled: true,
            full_list_max_files: 2,
            tree_max_depth: 4,
        };
        let out = cfg
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap()
            .expect("still yields a note");
        assert!(out.contains("very large"), "framed as too-large: {out}");
        assert!(out.contains("Discover"), "points at discovery: {out}");
        assert!(!out.contains("d0/"), "no directory lines dumped: {out}");
    }

    /// Disabled → no map, no work.
    #[tokio::test]
    async fn disabled_yields_none() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.rs", "fn a() {}\n");
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = OrientationConfig {
            enabled: false,
            full_list_max_files: 256,
            tree_max_depth: 4,
        };
        assert_eq!(
            cfg.assemble(&canon, SandboxConfig::default())
                .await
                .unwrap(),
            None
        );
    }
}
