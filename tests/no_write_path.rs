//! The read-only invariant, guarded at the source — now with one blessed exception.
//!
//! kaibo writes essentially nothing: no write path through kaish (enforced structurally —
//! see `tests/sandbox.rs`/`tests/worker.rs`, where a kaish write is refused), and no
//! handler-side `std::fs` write either. That has no runtime surface to probe — a handler
//! that called `std::fs::write` would just succeed — so we guard it at the source:
//! production code in `src/` must contain no filesystem-mutating call, save the **one
//! blessed site**.
//!
//! **The blessed site** (the sanctioned half of the read-only invariant amendment; see
//! `docs/kaibo-persistence-and-cli.md`): a single `create_dir_all` in `src/store.rs`, in
//! `create_state_dir`, carrying the marker comment on its own line. This is how the
//! persistence store creates its XDG state dir — a fixed, model-inaccessible path, only
//! after the containment check. Every other write kaibo makes goes through turso, which
//! this source scan never sees.
//!
//! The carve-out is deliberately narrow, and this test keeps its teeth:
//! - the exemption matches ONLY `create_dir_all(` in `store.rs` on a line carrying the
//!   marker — a `create_dir_all` anywhere else, or in `store.rs` without the marker, still
//!   fails (see the `teeth_*` unit tests);
//! - any OTHER forbidden call (`fs::write`, `File::create`, `remove_*`, `.write(`, …) still
//!   fails everywhere, including `store.rs`;
//! - exactly ONE blessed line may exist in the whole tree (`blessed_marker_appears_exactly_once`),
//!   so a second write site can't quietly ride in behind the marker.
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

/// The one blessed exception, pinned three ways (file + needle + marker) so it can't widen.
const BLESSED_FILE: &str = "store.rs";
const BLESSED_NEEDLE: &str = "create_dir_all(";
const BLESSED_MARKER: &str = "state-dir-create: blessed by the read-only invariant amendment";

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

/// Is this a blessed occurrence? Pinned to `create_dir_all(` in `store.rs` on a line whose
/// *raw* text (comment included) carries the marker — nothing else qualifies.
fn is_blessed(basename: &str, needle: &str, raw_line: &str) -> bool {
    basename == BLESSED_FILE && needle == BLESSED_NEEDLE && raw_line.contains(BLESSED_MARKER)
}

/// Forbidden needles found in one file's production code, honoring the blessed exception.
/// Pure over `(basename, source)` so the teeth tests can drive it with synthetic input.
fn scan_source(basename: &str, source: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for raw in production_code(source).lines() {
        let code = strip_line_comment(raw);
        for needle in FORBIDDEN {
            if code.contains(needle) && !is_blessed(basename, needle, raw) {
                hits.push(needle.to_string());
            }
        }
    }
    hits
}

/// Count of blessed marker lines in one file's production code — summed across the tree to
/// prove the carve-out is exactly one site.
fn blessed_count(source: &str) -> usize {
    production_code(source)
        .lines()
        .filter(|l| l.contains(BLESSED_MARKER))
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
        "kaibo writes nothing in production but the one blessed state-dir site — read-only \
         is unconditional. Found a filesystem-mutating call in non-test code:\n  {}\nIf this \
         is a deliberate, individually-mediated capability, update this guard's carve-out in \
         the same change (and its review).",
        violations.join("\n  ")
    );
}

/// The carve-out is exactly one site: exactly one blessed marker line in the whole tree.
/// A second write slipping in behind the marker fails here even if the scan above passes.
#[test]
fn blessed_marker_appears_exactly_once() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let total: usize = files
        .iter()
        .map(|f| blessed_count(&std::fs::read_to_string(f).expect("read source")))
        .sum();
    assert_eq!(
        total, 1,
        "expected exactly one blessed state-dir-creation site (in src/store.rs), found {total}"
    );
}

// --- Teeth: prove the exemption is narrow and the guard still bites ----------

/// The blessed line passes: `create_dir_all` in store.rs with the marker is exempt.
#[test]
fn teeth_blessed_line_is_exempt() {
    let src = format!("    std::fs::create_dir_all(dir) // {BLESSED_MARKER}\n");
    assert!(
        scan_source("store.rs", &src).is_empty(),
        "the marked create_dir_all in store.rs must be exempt"
    );
}

/// A `create_dir_all` in any OTHER file fails, even carrying the marker text.
#[test]
fn teeth_create_dir_all_elsewhere_fails() {
    let src = format!("    std::fs::create_dir_all(x) // {BLESSED_MARKER}\n");
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
/// the exemption is scoped to `create_dir_all(`, nothing else.
#[test]
fn teeth_other_write_in_store_fails() {
    let src = format!(
        "    std::fs::create_dir_all(dir) // {BLESSED_MARKER}\n    std::fs::write(p, b)?;\n"
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
