//! kaibo binds two sockets, never on a network address: the `kaibo login` loopback
//! callback, and the opt-in MCP Unix socket (`src/http_socket.rs`).
//!
//! The invariant was "stdio only — kaibo can read a filesystem, so it never binds a
//! socket". Signing in to a provider through the browser needs a redirect target, and
//! for a local tool that can only be a listener on this machine. So the invariant now
//! reads "kaibo binds no network address for MCP, and only at blessed sites", and this guard is what
//! makes "only" true: every bind call in `src/` production code must be a marked line
//! in a blessed file, and the count of marked lines is pinned. The MCP server binds
//! nothing; the callback lives in `src/oauth/callback.rs`, is reachable only from
//! `kaibo login`, refuses non-loopback addresses (`bind_loopback`), and closes after
//! one sign-in.
//!
//! Same scan shape as `tests/no_write_path.rs`: the production half of each file
//! (before its first `#[cfg(test)]`), needles matched with line comments stripped, the
//! marker read off the raw line.

use std::path::{Path, PathBuf};

/// Calls that open a listening or datagram socket.
const FORBIDDEN: &[&str] = &[
    "TcpListener::bind(",
    "UdpSocket::bind(",
    "UnixListener::bind(",
    "TcpSocket::new",
    "socket2::",
    ".listen(",
];

/// The blessed bind sites: (file, marker, the one needle the marker excuses).
const BLESSED: &[(&str, &str, &str)] = &[
    (
        "callback.rs",
        "loopback-bind: the one-shot sign-in callback",
        "TcpListener::bind(",
    ),
    (
        "http_socket.rs",
        "loopback-bind: the opt-in MCP Unix socket",
        "UnixListener::bind(",
    ),
];

/// Exactly one marked bind line per blessed site, tree-wide.
const EXPECTED_BLESSED_LINES: usize = 2;

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

fn production_code(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn blessed(basename: &str, raw: &str, needle: &str) -> bool {
    BLESSED
        .iter()
        .any(|(file, marker, n)| basename == *file && raw.contains(marker) && needle == *n)
}

fn scan_source(basename: &str, source: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for raw in production_code(source).lines() {
        let code = strip_line_comment(raw);
        for needle in FORBIDDEN {
            if code.contains(needle) && !blessed(basename, raw, needle) {
                hits.push(needle.to_string());
            }
        }
    }
    hits
}

fn marked_lines(basename: &str, source: &str) -> usize {
    production_code(source)
        .lines()
        .filter(|raw| {
            BLESSED
                .iter()
                .any(|(file, marker, _)| basename == *file && raw.contains(marker))
        })
        .count()
}

fn sources() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(!files.is_empty());
    files
        .into_iter()
        .map(|f| {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            (name, std::fs::read_to_string(&f).expect("read source"))
        })
        .collect()
}

#[test]
fn no_socket_is_bound_outside_the_blessed_sites() {
    let violations: Vec<String> = sources()
        .iter()
        .flat_map(|(name, text)| {
            scan_source(name, text)
                .into_iter()
                .map(move |n| format!("{name}: {n}"))
        })
        .collect();
    assert!(
        violations.is_empty(),
        "kaibo binds no socket but the `kaibo login` callback (src/oauth/callback.rs) and the \
         MCP Unix socket (src/http_socket.rs). Found:\n  \
         {}\nA new listener is a change to the stdio/socket invariant: update AGENTS.md and \
         this guard in the same change, with its review.",
        violations.join("\n  ")
    );
}

#[test]
fn exactly_the_blessed_bind_lines() {
    let total: usize = sources().iter().map(|(n, t)| marked_lines(n, t)).sum();
    assert_eq!(
        total, EXPECTED_BLESSED_LINES,
        "expected exactly one marked bind line per blessed site"
    );
}

#[test]
fn teeth_the_guard_bites() {
    let cb = "loopback-bind: the one-shot sign-in callback";
    let uds = "loopback-bind: the opt-in MCP Unix socket";
    let marked = format!("    TcpListener::bind(addr).await // {cb}\n");
    assert!(
        scan_source("callback.rs", &marked).is_empty(),
        "the marked callback bind passes"
    );
    assert!(
        !scan_source("server.rs", &marked).is_empty(),
        "the marker is pinned to its file"
    );
    assert!(
        !scan_source("callback.rs", "    TcpListener::bind(addr).await?;\n").is_empty(),
        "an unmarked bind fails even in the callback file"
    );
    let udp = format!("    UdpSocket::bind(addr) // {cb}\n");
    assert!(
        !scan_source("callback.rs", &udp).is_empty(),
        "the marker excuses only the TCP bind"
    );
    let unix = format!("    UnixListener::bind(path) // {uds}\n");
    assert!(
        scan_source("http_socket.rs", &unix).is_empty(),
        "the marked socket bind passes"
    );
    assert!(
        !scan_source("callback.rs", &unix).is_empty(),
        "the socket marker is pinned to http_socket.rs"
    );
    let tcp_in_socket_file = format!("    TcpListener::bind(addr) // {uds}\n");
    assert!(
        !scan_source("http_socket.rs", &tcp_in_socket_file).is_empty(),
        "the socket marker never excuses a TCP bind: no network address, ever"
    );
    assert!(
        scan_source(
            "x.rs",
            "fn p() {}\n#[cfg(test)]\nmod t { fn f() { TcpListener::bind(a); } }\n"
        )
        .is_empty(),
        "test modules may bind"
    );
}
