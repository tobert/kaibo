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
//! Size-gated: at or under `full_list_max_files` the whole list is injected; past it
//! the call is refused loudly (the directory-tree fallback for larger repos is the
//! next increment — for now, big repos are an explicit error, not a silent dump).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::sandbox::{KaishWorker, SandboxConfig};

/// Resolved `[orientation]` config: whether to inject the repo map, and the
/// file-count ceiling for the full-list form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrientationConfig {
    pub enabled: bool,
    /// At or under this many files, inject the complete list. Above it, the call is
    /// refused (dir-tree fallback not yet built).
    pub full_list_max_files: usize,
}

impl Default for OrientationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            full_list_max_files: 256,
        }
    }
}

impl OrientationConfig {
    /// Build the orientation block for `root`, or `None` when disabled or the repo
    /// is empty. Errors (loudly) when the repo is larger than `full_list_max_files`
    /// — that's a configuration decision the operator should see, not a silent skip.
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
        if n > self.full_list_max_files {
            bail!(
                "this project has {n} files, over [orientation] full_list_max_files \
                 ({}). The directory-tree map for larger repos isn't built yet — raise \
                 the limit, set [orientation] enabled = false, or point a cast that \
                 explores via `grep` at it.",
                self.full_list_max_files
            );
        }
        Ok(Some(render(&files)))
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

/// Render the file list into the injected block. The framing tells the model the
/// map is complete (so it skips discovery) and points it at the reads it should do
/// instead — the whole point is to convert "what's here?" turns into direct reads.
fn render(files: &[String]) -> String {
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

    /// Over the file-count ceiling, the call is refused loudly (not a silent skip).
    #[tokio::test]
    async fn refuses_a_repo_over_the_limit() {
        let dir = tempdir().unwrap();
        for i in 0..5 {
            write(dir.path(), &format!("f{i}.txt"), "x\n");
        }
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = OrientationConfig {
            enabled: true,
            full_list_max_files: 3, // below the 5 we wrote
        };
        let err = cfg
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("full_list_max_files"), "names the knob: {msg}");
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
        };
        assert_eq!(
            cfg.assemble(&canon, SandboxConfig::default())
                .await
                .unwrap(),
            None
        );
    }
}
