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
        // Captured before `sandbox` moves into the kernel: the map states the
        // read-whole ceiling in the operator's own configured terms, so the number a
        // model reads is the one its next `cat -n` will actually be cut at.
        let output_limit = sandbox.output_limit_bytes;
        let worker = KaishWorker::spawn_with(root, sandbox)
            .context("orientation: spawning the read-only kernel")?;
        let files = list_files(&worker).await?;
        if files.is_empty() {
            return Ok(None);
        }
        let n = files.len();
        if n <= self.full_list_max_files {
            let metrics = file_metrics(&worker, &files).await;
            return Ok(Some(render_full_list(&files, &metrics, output_limit)));
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

/// A file's size and line count — everything needed to say, exactly, whether one
/// `cat -n` returns it whole.
#[derive(Clone, Copy)]
pub(crate) struct FileMetrics {
    bytes: u64,
    lines: u64,
}

/// Measure the enumerated files through the *same* kernel that enumerated them — so a
/// number can never disagree with the file the model will read.
///
/// Two commands per chunk, `stat` for bytes and `wc -l` for lines, because the mark
/// this feeds is exact rather than estimated (see [`numbered_output_bytes`]). A file
/// missing from either result simply carries no numbers.
///
/// Best-effort by design: orientation is an enhancement, so a failure costs the
/// numbers, never the map. It is logged rather than swallowed, because a map that
/// silently lost its sizes looks identical to a repo that never had them. Note the
/// granularity: `stat` and `wc` both exit non-zero if *any* argument is missing, so
/// one unreadable path costs its whole chunk of 64 — the map keeps every path either
/// way, and the model falls back to reading whole and letting the result correct it.
///
/// Chunked because the paths ride the command line: a 256-file project would
/// otherwise build one very long script string.
async fn file_metrics(worker: &KaishWorker, files: &[String]) -> BTreeMap<String, FileMetrics> {
    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(rename = "FILE")]
        file: String,
        #[serde(rename = "SIZE")]
        size: Option<String>,
        #[serde(rename = "LINES")]
        lines: Option<String>,
    }

    async fn rows(worker: &KaishWorker, verb: &str, args: &str) -> Vec<Row> {
        let script = format!("{verb} {args}");
        let out = match worker.run(&script).await {
            Ok(out) if out.ok() => out,
            other => {
                tracing::warn!(
                    error = ?other.err(),
                    verb,
                    "orientation: measuring a chunk failed; those files keep their \
                     paths and lose their numbers"
                );
                return Vec::new();
            }
        };
        serde_json::from_str::<Vec<Row>>(out.stdout.trim()).unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                verb,
                "orientation: parsing a chunk's JSON failed; those files keep their \
                 paths and lose their numbers"
            );
            Vec::new()
        })
    }

    let mut metrics = BTreeMap::new();
    for chunk in files.chunks(64) {
        // Single-quote each path and strip any embedded quote: a path cannot break out
        // of the argument it sits in. A path that genuinely holds a quote is measured
        // under its stripped name, so it simply never matches on lookup and prints
        // without numbers — a miss, never a wrong number on the right file.
        let args = chunk
            .iter()
            .map(|f| format!("'{}'", f.replace('\'', "")))
            .collect::<Vec<_>>()
            .join(" ");

        let mut sizes = BTreeMap::new();
        for r in rows(worker, "stat --json", &args).await {
            if let Some(n) = r.size.and_then(|s| s.parse::<u64>().ok()) {
                sizes.insert(r.file, n);
            }
        }
        for r in rows(worker, "wc -l --json", &args).await {
            // `wc` appends a "total" row when given several files; it names no real
            // path, so the join below drops it.
            if let (Some(&bytes), Some(lines)) = (
                sizes.get(&r.file),
                r.lines.and_then(|l| l.parse::<u64>().ok()),
            ) {
                metrics.insert(r.file, FileMetrics { bytes, lines });
            }
        }
    }
    metrics
}

/// What one `cat -n FILE` actually delivers: the file's bytes plus its line numbering.
///
/// Exact, not estimated, and the reason this is worth being exact about: `cat -n`
/// prefixes each line with a right-aligned number and a tab, so the delivered bytes
/// exceed the file's own size by `lines × (width + 1)`. Measured on this repo, the
/// overhead is exactly 7 bytes per line at the usual 6-column width.
///
/// A fraction-of-the-cap heuristic gets this wrong in the one direction that matters.
/// Four fifths of a 64 KiB cap would call a 20 KB file of two-byte lines readable
/// whole, when numbering takes it to 92 KB and it truncates — the map would be
/// promising something false, which is worse than saying nothing. Caught by the
/// cross-family review of this change (DeepSeek, 2026-08-12), which also supplied that
/// falsifier.
fn numbered_output_bytes(m: FileMetrics) -> u64 {
    let width = m.lines.to_string().len().max(6) as u64;
    m.bytes + m.lines * (width + 1)
}

/// Bytes as whole kilobytes, floored, minimum 1 — the unit the read-whole ceiling is
/// stated in, so a model compares two numbers instead of counting digits.
fn kb(bytes: u64) -> u64 {
    (bytes / 1024).max(1)
}

/// Render the complete file list into the injected block. The framing tells the
/// model the map is complete (so it skips discovery) and points it at the reads it
/// should do instead — the whole point is to convert "what's here?" turns into
/// direct reads.
///
/// **Sizes are here to remove a guess.** Measured over 16,105 real reads (a month of
/// traces, 2026-08-12), the median read delivered 1,837 bytes — about 45 lines —
/// while whole-file reads truncated only 1.8% of the time. Models were reading in
/// small windows to avoid a risk that almost never materialized, because from inside
/// the sandbox a file's size is unknown until it has been read. Publishing the size
/// turns that judgment call into a comparison, and marking the files that would
/// truncate puts the exception on the line it applies to instead of in prose the
/// reader has to hold.
///
/// The mark is computed per file from its real bytes and lines
/// ([`numbered_output_bytes`]), never from a fraction of the cap, so an unmarked file
/// is one that *will* come back whole rather than one that probably will.
fn render_full_list(
    files: &[String],
    metrics: &BTreeMap<String, FileMetrics>,
    output_limit: usize,
) -> String {
    let mut s = String::with_capacity(80 + files.iter().map(|f| f.len() + 16).sum::<usize>());
    s.push_str(
        "PROJECT FILES. The project's complete file list (read-only; hidden config \
         included, build/VCS dirs excluded). You already have the whole layout here, \
         so go straight to reading the files the question touches with `cat -n FILE`, \
         and use `grep -rn` to find where something lives inside them.\n",
    );
    if !metrics.is_empty() {
        s.push_str(
            "Each file's size is given, and every file listed WITHOUT a mark comes back \
             WHOLE from one `cat -n FILE`. That is most of this list, so read those \
             whole and never guess. The files that would not fit are marked `read in \
             spans`; for those, `grep -n SYMBOL FILE` first, then read a wide span \
             around each hit with `cat -n FILE | sed -n '1200,2400p'`.\n",
        );
    }
    // One column, so the sizes line up and the marked files stand out down the page.
    let width = files.iter().map(String::len).max().unwrap_or(0).min(72);
    for f in files {
        s.push_str("  ");
        match metrics.get(f) {
            Some(&m) => {
                let mark = if numbered_output_bytes(m) > output_limit as u64 {
                    "   read in spans"
                } else {
                    ""
                };
                s.push_str(&format!("{f:<width$}  {:>5} KB{mark}\n", kb(m.bytes)));
            }
            None => {
                s.push_str(f);
                s.push('\n');
            }
        }
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
        s.push_str(&format!("  ./  {}\n", count_phrase(root.direct_files)));
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
        self.direct_files
            + self
                .children
                .values()
                .map(DirNode::total_files)
                .sum::<usize>()
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

    /// Every listed file carries its size, and the block promises a whole read for
    /// every file it does not mark.
    ///
    /// This is the feature's whole point: measured over 16,105 real reads, the median
    /// delivered 1,837 bytes while whole-file reads truncated 1.8% of the time —
    /// models hedge because a file's size is unknowable from inside the sandbox until
    /// it has been read. A map without sizes leaves that guess in place.
    #[tokio::test]
    async fn the_file_list_publishes_sizes_and_promises_a_whole_read() {
        let dir = tempdir().unwrap();
        write(dir.path(), "small.rs", &"x\n".repeat(100)); // 200 B
        let canon = std::fs::canonicalize(dir.path()).unwrap();

        let out = OrientationConfig::default()
            .assemble(&canon, SandboxConfig::default())
            .await
            .unwrap()
            .expect("a non-empty repo yields a map");

        assert!(
            out.contains("small.rs") && out.contains("KB"),
            "each file is listed with a size: {out}"
        );
        assert!(
            out.contains("WITHOUT a mark comes back WHOLE"),
            "the block states the promise the mark makes exact: {out}"
        );
        let line = out
            .lines()
            .find(|l| l.contains("small.rs"))
            .expect("listed");
        assert!(
            !line.contains("read in spans"),
            "a 200-byte file fits: {line}"
        );
    }

    /// A file that would truncate is marked on its own line; one that fits is not.
    /// The exception rides the line it applies to, so a model reading the list never
    /// has to hold a rule in its head while scanning.
    #[tokio::test]
    async fn only_files_that_would_truncate_are_marked_for_span_reading() {
        let dir = tempdir().unwrap();
        let limit = 4096;
        write(dir.path(), "tiny.rs", &"x\n".repeat(64)); // 128 B, 64 lines -> 576
        write(dir.path(), "huge.rs", &"x\n".repeat(8192)); // 16 KB, 8192 lines
        let canon = std::fs::canonicalize(dir.path()).unwrap();

        let out = OrientationConfig::default()
            .assemble(
                &canon,
                SandboxConfig {
                    output_limit_bytes: limit,
                    ..SandboxConfig::default()
                },
            )
            .await
            .unwrap()
            .expect("a non-empty repo yields a map");

        let huge = out.lines().find(|l| l.contains("huge.rs")).expect("listed");
        let tiny = out.lines().find(|l| l.contains("tiny.rs")).expect("listed");
        assert!(huge.contains("read in spans"), "would truncate: {huge}");
        assert!(!tiny.contains("read in spans"), "fits whole: {tiny}");
    }

    /// The falsifier the cross-family review supplied, pinned so it cannot come back.
    ///
    /// A fraction-of-the-cap heuristic (the first version used four fifths) calls a
    /// 20 KB file of two-byte lines readable whole. `cat -n` numbering takes it to
    /// 92 KB against a 64 KiB cap, so it truncates — the map would have promised
    /// something false, which is worse than saying nothing. The mark is computed from
    /// real bytes and lines instead, so short lines are counted rather than assumed.
    #[test]
    fn short_lines_are_counted_not_assumed() {
        let cap = 1usize << 16;
        let short = FileMetrics {
            bytes: 20 * 1024,
            lines: 10 * 1024,
        };
        assert!(
            (short.bytes as usize) < cap / 5 * 4,
            "guard: this file is UNDER the old four-fifths ceiling, which is the trap"
        );
        assert!(
            numbered_output_bytes(short) > cap as u64,
            "a 20 KB file of two-byte lines does not survive `cat -n` at a 64 KiB cap"
        );
        // The same byte count in long lines does fit, so the rule tracks shape.
        let long = FileMetrics {
            bytes: 20 * 1024,
            lines: 250,
        };
        assert!(
            numbered_output_bytes(long) < cap as u64,
            "the same bytes in long lines fit whole"
        );
    }

    /// Sizes are an enhancement: without them the map still lists every path, so a
    /// stat failure costs the sizes and never the orientation.
    #[test]
    fn a_map_without_sizes_still_lists_every_file() {
        let files = vec!["src/main.rs".to_string(), "README.md".to_string()];
        let out = render_full_list(&files, &BTreeMap::new(), 1 << 16);
        assert!(
            out.contains("src/main.rs") && out.contains("README.md"),
            "{out}"
        );
        assert!(
            !out.contains("KB"),
            "with no sizes the block makes no size claims: {out}"
        );
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
        assert!(
            out.contains("PROJECT STRUCTURE"),
            "framed as structure: {out}"
        );
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
