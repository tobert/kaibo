//! The unconditional read-only invariant, guarded at the source.
//!
//! kaibo writes *nothing, anywhere*: no write path through kaish (enforced
//! structurally — see `tests/sandbox.rs`/`tests/worker.rs`, where a kaish write is
//! refused), and **no handler-side write either**. The latter has no runtime surface to
//! probe — a handler that called `std::fs::write` would just succeed — so we guard it at
//! the source: production code in `src/` must contain no filesystem-mutating call.
//!
//! This is the teeth behind "read-only is unconditional" in AGENTS.md. It *would* fire
//! on the old `generate_image` capability (its `write_artifact` did `std::fs::create_dir_all`
//! + `std::fs::write` to the out-dir) — that whole write path is gone. If kaibo ever
//! grows a deliberate, individually-mediated *capability tool* that records an artifact,
//! that's a conscious exception: this test is updated in the same change that adds it and
//! its review, never silently.
//!
//! Scope: the *production* half of each `src/**.rs` — everything before the file's first
//! `#[cfg(test)]` (test modules legitimately write fixtures). Line comments are stripped so
//! prose naming these calls doesn't trip it.

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
/// (the trailing test module), with line comments stripped so prose can name a forbidden
/// call without tripping the scan.
fn production_code(src: &str) -> String {
    let prod = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };
    prod.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        let prod = production_code(&text);
        for needle in FORBIDDEN {
            if prod.contains(needle) {
                violations.push(format!("{}: {needle}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "kaibo writes nothing in production — read-only is unconditional. Found a \
         filesystem-mutating call in non-test code:\n  {}\nIf this is a deliberate, \
         individually-mediated capability tool, update this guard in the same change \
         (and its review).",
        violations.join("\n  ")
    );
}
