//! The read-only invariant, guarded at the source — now with two blessed exceptions.
//!
//! kaibo writes essentially nothing: no write path through kaish (enforced structurally —
//! see `tests/sandbox.rs`/`tests/worker.rs`, where a kaish write is refused), and no
//! handler-side `std::fs` write either. That has no runtime surface to probe — a handler
//! that called `std::fs::write` would just succeed — so we guard it at the source:
//! production code in `src/` must contain no filesystem-mutating call, save the **two
//! blessed sites** below.
//!
//! **Blessed site 1** (the sanctioned first half of the read-only invariant amendment; see
//! `docs/kaibo-persistence-and-cli.md`): a single `create_dir_all` in `src/store.rs`, in
//! `create_state_dir`, carrying the marker comment on its own line. This is how the
//! persistence store creates its XDG state dir — a fixed, model-inaccessible path, only
//! after the containment check.
//!
//! **Blessed site 2** (the second and last amendment; see `docs/devlog.md` (2026-07-25) and
//! `src/cas.rs`'s module doc): the media CAS, a content-addressed store of generated
//! artifacts. Unlike `store.rs`, one write-only object write there touches the filesystem
//! at **three** distinct seams, not one — `create_dir_all` for the two-level hex shard
//! directory, `OpenOptions::new()...create_new(true).open(...)` for the `O_EXCL` file
//! open, and `.write_all(...)` for the bytes themselves — so this guard cannot pin `cas.rs`
//! to a single `(file, needle)` pair the way `store.rs`'s single `create_dir_all` allowed.
//! That is *why* this test generalizes the carve-out (below) from "one marker, one needle,
//! one file" to "any number of individually marked lines, in a small fixed set of files" —
//! the safety property was never "exactly one needle is blessed," it was always "every
//! blessed line is deliberate, visible, and can't silently multiply." The per-line,
//! per-file pinning below keeps exactly that property; only the *count* it must satisfy
//! grows from one to four (1 in `store.rs` + 3 in `cas.rs`).
//!
//! The carve-out is deliberately narrow, and this test keeps its teeth:
//! - a blessed line is exempted **only** when its file is one of the two blessed files
//!   *and* the line's own raw text carries that file's marker — nothing else qualifies
//!   (see the `teeth_*` unit tests, including the new ones proving both directions for
//!   `cas.rs` and that the two files' markers don't cross-pollinate);
//! - any OTHER forbidden call (`fs::write`, `File::create`, `remove_*`, `.write(`, …) still
//!   fails everywhere, including inside the two blessed files, on any line lacking that
//!   file's own marker;
//! - the total number of blessed marker lines tree-wide is pinned to an **exact count**
//!   (`EXPECTED_BLESSED_LINES`, currently 4), so a new write site — in an existing blessed
//!   file or a new one — can't quietly ride in behind an existing marker without this test
//!   failing (`blessed_marker_count_matches_exactly`).
//!
//! It *would* still fire on the old `generate_image` capability (its `write_artifact` did a
//! `create_dir_all` + `write` to an out-dir). A future deliberate capability that records an
//! artifact is a conscious exception updated here in the same change and its review, never
//! silently.
//!
//! Scope: the *production* half of each `src/**.rs` — everything before the file's first
//! `#[cfg(test)]` (test modules legitimately write fixtures). Line comments are stripped for
//! needle matching so prose naming these calls doesn't trip it (the blessed marker is read
//! off the *raw* line, so stripping can't hide it).

use std::path::{Path, PathBuf};

/// Filesystem-mutating calls that must not appear in production code.
const FORBIDDEN: &[&str] = &[
    "fs::write(",
    "create_dir(",
    "create_dir_all(",
    "File::create(",
    "OpenOptions::",
    "remove_file(",
    "remove_dir(",
    "remove_dir_all(",
    "fs::rename(",
    ".write_all(",
    ".write(", // io::Write::write — broad on purpose; production code has no reason to.
];

/// The blessed exceptions, each pinned to one file and one marker comment. A line is
/// exempt only when its own file matches an entry here *and* its own raw text contains
/// that entry's marker — the needle itself is no longer part of the pin, because `cas.rs`
/// legitimately blesses three *different* needles (`create_dir_all(`, `OpenOptions::`
/// alongside `.write(` on one line, and `.write_all(`) under one marker; exemption is a
/// per-line property already (the marker has to be ON the offending line), so needle-
/// pinning added no additional narrowness store.rs's single needle didn't already get for
/// free.
const BLESSED: &[(&str, &str)] = &[
    (
        "store.rs",
        "state-dir-create: blessed by the read-only invariant amendment",
    ),
    (
        "cas.rs",
        "media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)",
    ),
];

/// The exact number of blessed marker lines expected tree-wide: 1 in `store.rs`
/// (`create_state_dir`'s `create_dir_all`) + 3 in `cas.rs` (the shard-dir `create_dir_all`,
/// the `create_new` open, and the `write_all` of the bytes — see `src/cas.rs`'s module
/// doc and the count breakdown in this file's module doc). Pinned so a new write site
/// can't silently ride in behind an existing marker.
const EXPECTED_BLESSED_LINES: usize = 4;

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The production half of a source file: everything before the first `#[cfg(test)]`
/// (the trailing test module).
fn production_code(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// A source line up to its first `//` — the form we match needles against, so prose in a
/// trailing comment can't trip the scan.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Is this a blessed occurrence? Exempt when `basename` matches one of [`BLESSED`]'s
/// files and `raw_line` (comment included) carries *that file's* marker text. No needle
/// pin: blessing is inherently per-line (the marker must be on the offending line
/// itself), so a marker on the right line already can't drift onto an unrelated line
/// carrying a different forbidden call — see the module doc for why `cas.rs` needs this
/// generalization over `store.rs`'s original single `(file, needle, marker)` pin.
fn is_blessed(basename: &str, raw_line: &str) -> bool {
    BLESSED
        .iter()
        .any(|(file, marker)| basename == *file && raw_line.contains(marker))
}

/// Forbidden needles found in one file's production code, honoring the blessed
/// exceptions. Pure over `(basename, source)` so the teeth tests can drive it with
/// synthetic input.
fn scan_source(basename: &str, source: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for raw in production_code(source).lines() {
        let code = strip_line_comment(raw);
        for needle in FORBIDDEN {
            if code.contains(needle) && !is_blessed(basename, raw) {
                hits.push(needle.to_string());
            }
        }
    }
    hits
}

/// Count of blessed marker lines in one file's production code — summed across the tree
/// to prove the carve-out is exactly [`EXPECTED_BLESSED_LINES`] sites, no more.
fn blessed_count(basename: &str, source: &str) -> usize {
    production_code(source)
        .lines()
        .filter(|raw| is_blessed(basename, raw))
        .count()
}

fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn no_handler_side_write_path_in_production_code() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(!files.is_empty(), "expected to scan some src files");

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source");
        for needle in scan_source(&basename(file), &text) {
            violations.push(format!("{}: {needle}", file.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "kaibo writes nothing in production but the two blessed sites (store.rs's state dir, \
         cas.rs's media CAS) — read-only is unconditional. Found a filesystem-mutating call in \
         non-test code:\n  {}\nIf this is a deliberate, individually-mediated capability, update \
         this guard's carve-out in the same change (and its review).",
        violations.join("\n  ")
    );
}

/// The carve-out is pinned to an exact count: `EXPECTED_BLESSED_LINES` blessed marker
/// lines in the whole tree, no more, no fewer. A new write site slipping in behind an
/// existing marker (in `store.rs` or `cas.rs`) — or a third blessed file appearing without
/// review — fails here even if the scan above passes.
#[test]
fn blessed_marker_count_matches_exactly() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let total: usize = files
        .iter()
        .map(|f| blessed_count(&basename(f), &std::fs::read_to_string(f).expect("read source")))
        .sum();
    assert_eq!(
        total, EXPECTED_BLESSED_LINES,
        "expected exactly {EXPECTED_BLESSED_LINES} blessed write sites tree-wide (1 in \
         src/store.rs + 3 in src/cas.rs), found {total}"
    );
}

// --- Teeth: prove the exemption is narrow and the guard still bites ----------

/// The blessed line passes: `create_dir_all` in store.rs with its marker is exempt.
#[test]
fn teeth_blessed_line_is_exempt() {
    let src = format!(
        "    std::fs::create_dir_all(dir) // {}\n",
        BLESSED[0].1
    );
    assert!(
        scan_source("store.rs", &src).is_empty(),
        "the marked create_dir_all in store.rs must be exempt"
    );
}

/// A `create_dir_all` in any OTHER file fails, even carrying store.rs's marker text.
#[test]
fn teeth_create_dir_all_elsewhere_fails() {
    let src = format!("    std::fs::create_dir_all(x) // {}\n", BLESSED[0].1);
    assert!(
        !scan_source("server.rs", &src).is_empty(),
        "create_dir_all outside store.rs must fail even with the marker"
    );
}

/// A `create_dir_all` in store.rs WITHOUT the marker fails — the marker is load-bearing.
#[test]
fn teeth_unmarked_create_dir_all_in_store_fails() {
    let src = "    std::fs::create_dir_all(dir)?;\n";
    assert!(
        !scan_source("store.rs", src).is_empty(),
        "an unmarked create_dir_all in store.rs must fail"
    );
}

/// A DIFFERENT forbidden call in store.rs fails even when a blessed line is also present —
/// the exemption is per-line, not per-file.
#[test]
fn teeth_other_write_in_store_fails() {
    let src = format!(
        "    std::fs::create_dir_all(dir) // {}\n    std::fs::write(p, b)?;\n",
        BLESSED[0].1
    );
    let v = scan_source("store.rs", &src);
    assert!(
        v.iter().any(|s| s.contains("fs::write(")),
        "a non-blessed write in store.rs must still fail, got: {v:?}"
    );
}

/// A forbidden call inside a `#[cfg(test)]` module is ignored (fixtures may write).
#[test]
fn teeth_test_module_writes_are_ignored() {
    let src = "fn prod() {}\n#[cfg(test)]\nmod t { fn f() { std::fs::write(p, b); } }\n";
    assert!(
        scan_source("whatever.rs", src).is_empty(),
        "writes in a test module must be ignored"
    );
}

/// The three distinct blessed needles in `cas.rs` are each exempt on their own marked
/// line: the shard `create_dir_all`, the `OpenOptions::`/`.write(` builder chain (both
/// needles hit on one physical line, one marker), and the `.write_all(` of the bytes.
#[test]
fn teeth_cas_blessed_lines_are_exempt() {
    let marker = BLESSED[1].1;
    let src = format!(
        "    std::fs::create_dir_all(&shard) // {marker}\n\
         \n\
         fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {{\n\
         \x20   let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path) // {marker}\n\
         \x20       ?;\n\
         \x20   file.write_all(bytes) // {marker}\n\
         }}\n"
    );
    let hits = scan_source("cas.rs", &src);
    assert!(
        hits.is_empty(),
        "all three marked cas.rs write sites must be exempt, got: {hits:?}"
    );
}

/// A forbidden call in `cas.rs` WITHOUT the marker on its own line still fails — for each
/// of the three needle shapes cas.rs legitimately uses.
#[test]
fn teeth_forbidden_call_in_cas_without_marker_fails() {
    assert!(
        !scan_source("cas.rs", "    std::fs::create_dir_all(&shard)?;\n").is_empty(),
        "an unmarked create_dir_all in cas.rs must fail"
    );
    assert!(
        !scan_source(
            "cas.rs",
            "    std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;\n"
        )
        .is_empty(),
        "an unmarked OpenOptions open in cas.rs must fail"
    );
    assert!(
        !scan_source("cas.rs", "    file.write_all(bytes)?;\n").is_empty(),
        "an unmarked write_all in cas.rs must fail"
    );
}

/// `cas.rs`'s marker text appearing in some OTHER file does not exempt anything there —
/// the pin is per-file, not a tree-wide magic comment.
#[test]
fn teeth_cas_marker_in_wrong_file_fails() {
    let marker = BLESSED[1].1;
    let src = format!("    std::fs::create_dir_all(x) // {marker}\n");
    assert!(
        !scan_source("server.rs", &src).is_empty(),
        "cas.rs's marker must not exempt a write in an unrelated file"
    );
}

/// The two files' markers don't cross-pollinate: `store.rs`'s marker on a forbidden line
/// in `cas.rs` does not exempt it, and vice versa — each blessed file only recognizes its
/// own marker text.
#[test]
fn teeth_wrong_marker_for_file_fails() {
    let store_marker = BLESSED[0].1;
    let cas_marker = BLESSED[1].1;

    let src_cas = format!("    std::fs::create_dir_all(x) // {store_marker}\n");
    assert!(
        !scan_source("cas.rs", &src_cas).is_empty(),
        "store.rs's marker must not exempt a write in cas.rs"
    );

    let src_store = format!("    std::fs::create_dir_all(x) // {cas_marker}\n");
    assert!(
        !scan_source("store.rs", &src_store).is_empty(),
        "cas.rs's marker must not exempt a write in store.rs"
    );
}
