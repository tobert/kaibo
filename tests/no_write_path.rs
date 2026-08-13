//! The read-only invariant, guarded at the source — now with three blessed exceptions.
//!
//! kaibo writes essentially nothing: no write path through kaish (enforced structurally —
//! see `tests/sandbox.rs`/`tests/worker.rs`, where a kaish write is refused), and no
//! handler-side `std::fs` write either. That has no runtime surface to probe — a handler
//! that called `std::fs::write` would just succeed — so we guard it at the source:
//! production code in `src/` must contain no filesystem-mutating call, save the **three
//! blessed sites** below.
//!
//! **Blessed site 1** (the sanctioned first half of the read-only invariant amendment; see
//! `docs/kaibo-persistence-and-cli.md`): a single `create_dir_all` in `src/store.rs`, in
//! `create_state_dir`, carrying the marker comment on its own line. This is how the
//! persistence store creates its XDG state dir — a fixed, model-inaccessible path, only
//! after the containment check.
//!
//! **Blessed site 2** (see `docs/devlog.md` (2026-07-25) and
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
//! grows. It went from one to four when `cas.rs` arrived (1 in `store.rs` + 3 there), and
//! to five with blessed site 3 below — `EXPECTED_BLESSED_LINES` is the current number and
//! this sentence is the history, so read the constant, not this paragraph.
//!
//! **Blessed site 3** (2026-08-07): `kaibo cas read` writing an artifact's bytes to
//! **stdout**. Different in kind from the other two, and worth saying why it is here at
//! all. It creates nothing on any filesystem — stdout is a descriptor the operator's shell
//! already opened and aimed, so this store still has no destination parameter anywhere and
//! no model can steer it. It is blessed rather than exempted-by-cleverness because the
//! needle (`.write_all(`) cannot tell a file from a pipe, and a guard that tried to would
//! be the kind of regex that fails silently in the wrong direction. The bounded,
//! base64-averse rules an MCP read follows exist to protect a *context window*; a pipe has
//! no such budget, and a caller who asked for an image by digest expects its bytes — so
//! this front door serves them, visibly and countably (Amy, 2026-08-07: "we do know when
//! we might output binary and the caller likely knows they're gonna get binary").
//!
//! The carve-out is deliberately narrow, and this test keeps its teeth:
//! - a blessed line is exempted **only** when its file is one of the blessed files
//!   *and* the line's own raw text carries that file's marker *and* the call is one that
//!   file's marker excuses — nothing else qualifies (see the `teeth_*` unit tests,
//!   including the ones proving both directions for `cas.rs` and that the files' markers
//!   don't cross-pollinate);
//! - any OTHER forbidden call (`fs::write`, `File::create`, `remove_*`, `.write(`, …) still
//!   fails everywhere, including inside the blessed files, on any line lacking that
//!   file's own marker;
//! - the total number of blessed marker lines tree-wide is pinned to an **exact count**
//!   (`EXPECTED_BLESSED_LINES`, currently 5), so a new write site — in an existing blessed
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
const BLESSED: &[Blessed] = &[
    Blessed {
        file: "store.rs",
        marker: "state-dir-create: blessed by the read-only invariant amendment",
        needles: &["create_dir_all("],
    },
    Blessed {
        file: "cas.rs",
        marker: "media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)",
        // One object write touches three seams; the `create_new` open hits two needles on
        // one physical line.
        needles: &[
            "create_dir_all(",
            "OpenOptions::",
            ".write(",
            ".write_all(",
        ],
    },
    Blessed {
        file: "cli.rs",
        marker: "cas-stdout-bytes: blessed by the media CAS invariant amendment (AGENTS.md)",
        // Handing bytes to an already-open descriptor, and nothing else. A path-taking
        // call is a different capability and must fail here even with the marker on it.
        needles: &[".write_all("],
    },
];

/// One blessed write site: which file may carry it, the marker its line must show, and
/// **which** forbidden calls that marker excuses.
///
/// The needle list was reintroduced on 2026-08-07 (it had been dropped when `cas.rs`
/// needed three needles under one marker). Without it, a marker excuses *any* forbidden
/// call on its line, so pasting `cli.rs`'s marker onto an `fs::write(out_path, …)` would
/// launder a caller-named destination past this guard — and the CLI is exactly where a
/// "just write it to a file for me" flag would be proposed. Caught by
/// `teeth_cli_writes_without_the_marker_fail`.
struct Blessed {
    file: &'static str,
    marker: &'static str,
    needles: &'static [&'static str],
}

/// The exact number of blessed marker lines expected tree-wide: 1 in `store.rs`
/// (`create_state_dir`'s `create_dir_all`) + 3 in `cas.rs` (the shard-dir `create_dir_all`,
/// the `create_new` open, and the `write_all` of the bytes — see `src/cas.rs`'s module
/// doc and the count breakdown in this file's module doc) + 1 in `cli.rs` (`kaibo cas
/// read` handing an artifact's bytes to stdout). Pinned so a new write site can't
/// silently ride in behind an existing marker.
const EXPECTED_BLESSED_LINES: usize = 5;

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

/// Is this needle blessed on this line? All three must hold: the file carries a blessing,
/// the line's own raw text (comment included) shows that blessing's marker, and the needle
/// is one the blessing actually excuses.
fn is_blessed_needle(basename: &str, raw_line: &str, needle: &str) -> bool {
    BLESSED.iter().any(|b| {
        basename == b.file && raw_line.contains(b.marker) && b.needles.contains(&needle)
    })
}

/// Does this line carry a blessing marker at all? Used for the tree-wide count, which
/// pins how many blessed *lines* exist regardless of how many needles each excuses.
fn is_blessed_line(basename: &str, raw_line: &str) -> bool {
    BLESSED
        .iter()
        .any(|b| basename == b.file && raw_line.contains(b.marker))
}

/// Forbidden needles found in one file's production code, honoring the blessed
/// exceptions. Pure over `(basename, source)` so the teeth tests can drive it with
/// synthetic input.
fn scan_source(basename: &str, source: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for raw in production_code(source).lines() {
        let code = strip_line_comment(raw);
        for needle in FORBIDDEN {
            if code.contains(needle) && !is_blessed_needle(basename, raw, needle) {
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
        .filter(|raw| is_blessed_line(basename, raw))
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
         src/store.rs + 3 in src/cas.rs + 1 in src/cli.rs), found {total}"
    );
}

// --- Teeth: prove the exemption is narrow and the guard still bites ----------

/// The blessed line passes: `create_dir_all` in store.rs with its marker is exempt.
#[test]
fn teeth_blessed_line_is_exempt() {
    let src = format!(
        "    std::fs::create_dir_all(dir) // {}\n",
        BLESSED[0].marker
    );
    assert!(
        scan_source("store.rs", &src).is_empty(),
        "the marked create_dir_all in store.rs must be exempt"
    );
}

/// A `create_dir_all` in any OTHER file fails, even carrying store.rs's marker text.
#[test]
fn teeth_create_dir_all_elsewhere_fails() {
    let src = format!("    std::fs::create_dir_all(x) // {}\n", BLESSED[0].marker);
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
        BLESSED[0].marker
    );
    let v = scan_source("store.rs", &src);
    assert!(
        v.iter().any(|s| s.contains("fs::write(")),
        "a non-blessed write in store.rs must still fail, got: {v:?}"
    );
}

/// The blessed stdout write in `cli.rs` is exempt on its own marked line.
#[test]
fn teeth_cli_stdout_bytes_line_is_exempt() {
    let src = format!("            out.write_all(&bytes[start..end]) // {}\n", BLESSED[2].marker);
    assert!(
        scan_source("cli.rs", &src).is_empty(),
        "the marked stdout write in cli.rs must be exempt"
    );
}

/// ...and an UNMARKED write in `cli.rs` still fails, as does the marked one anywhere else.
/// The CLI is the front door most likely to grow a "just write it to a file for me" flag,
/// so this is the direction that matters.
#[test]
fn teeth_cli_writes_without_the_marker_fail() {
    assert!(
        !scan_source("cli.rs", "    std::fs::write(out_path, &bytes)?;\n").is_empty(),
        "an unmarked fs::write in cli.rs must fail — a --out flag is a new capability, \
         not a variation on serving stdout"
    );
    let marked = format!("    std::fs::write(out_path, &bytes) // {}\n", BLESSED[2].marker);
    assert!(
        !scan_source("cli.rs", &marked).is_empty(),
        "cli.rs's marker blesses the stdout needle only — it must not launder an \
         fs::write onto a caller-named path"
    );
    let elsewhere = format!("    out.write_all(&bytes) // {}\n", BLESSED[2].marker);
    assert!(
        !scan_source("server.rs", &elsewhere).is_empty(),
        "cli.rs's marker must not exempt a write in another file"
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
    let marker = BLESSED[1].marker;
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
    let marker = BLESSED[1].marker;
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
    let store_marker = BLESSED[0].marker;
    let cas_marker = BLESSED[1].marker;

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
