//! The bearer token that guards kaibo's HTTP transport.
//!
//! One file at a fixed XDG path, `$XDG_STATE_HOME/kaibo/http-token` (else
//! `~/.local/state/kaibo/http-token`), next to the persistence store and never in the
//! config dir: people keep `~/.config` in dotfiles repos, and a token there gets committed.
//! The file holds one line, 32 random bytes as unpadded base64url.
//!
//! The server re-reads the file on every request, so `kaibo http-token --rotate` takes
//! effect on the next request with no restart, a deleted file fails closed, and a client
//! that reads the file on each connect (Claude Code's `headersHelper`, OpenCode's
//! `{file:...}`) never needs updating.
//!
//! This is kaibo's third write surface, under the same discipline as the store and the
//! media CAS: a fixed path no model can aim, refused if it resolves into an allowed tree,
//! created only through individually blessed lines (`tests/no_write_path.rs`). The token
//! value is never logged and never appears in `kaibo://config`, which shows the path only.
//! Rotation writes a fresh temp file beside the token and renames it over, so a reader
//! never sees half a token. A crash between the two leaves a stray `http-token.*.tmp`,
//! which is harmless and which kaibo, having no delete path, leaves for the operator.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;

/// The token file's name inside the state dir.
const TOKEN_FILE: &str = "http-token";

/// Why the token could not be read or created.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The token path resolves inside an allowed tree, where a model could read it.
    #[error(
        "refused the HTTP token path {0}: it is inside an allowed tree, so the model team \
         could read it. Move the state dir with XDG_STATE_HOME, or narrow --root / \
         --allow-path so it no longer covers the state dir."
    )]
    PathInAllowedTree(String),
    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set.
    #[error(
        "no place for the HTTP token: neither XDG_STATE_HOME nor HOME is set. Set one of \
         them before enabling [server.http]."
    )]
    NoStateDir,
    /// The file exists but holds no token.
    #[error(
        "the HTTP token file {0} is empty. Run `kaibo http-token --rotate` to write a new token."
    )]
    Empty(String),
    /// Any filesystem failure, with the path it hit.
    #[error("HTTP token file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> TokenError {
    TokenError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The fixed token path: `$XDG_STATE_HOME/kaibo/http-token`, else
/// `~/.local/state/kaibo/http-token`.
pub fn default_token_path() -> Result<PathBuf, TokenError> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(xdg).join("kaibo").join(TOKEN_FILE));
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("kaibo")
                .join(TOKEN_FILE)
        })
        .ok_or(TokenError::NoStateDir)
}

/// Refuse a token path that resolves into any allowed tree. Runs before any disk write, on
/// the full path (parent resolved through symlinks, then the file name), like the store's
/// guard.
pub fn check_containment(path: &Path, allowed_trees: &[&Path]) -> Result<(), TokenError> {
    let name = path.file_name().unwrap_or_default();
    let resolved = crate::store::resolve_existing_parent(path).join(name);
    for tree in allowed_trees {
        let tree = tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf());
        if resolved.starts_with(&tree) {
            return Err(TokenError::PathInAllowedTree(path.display().to_string()));
        }
    }
    Ok(())
}

/// A fresh token: 32 random bytes as unpadded base64url (43 characters).
fn mint() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Write `token` to a path that must not exist yet, readable by the owner only. Every
/// token write in kaibo goes through here.
fn write_new(path: &Path, token: &str) -> std::io::Result<()> {
    let mut opts = OpenOptions::new().write(true).create_new(true).clone(); // http-token-write: blessed by the HTTP token invariant amendment (AGENTS.md)
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(format!("{token}\n").as_bytes()) // http-token-write: blessed by the HTTP token invariant amendment (AGENTS.md)
}

/// Read the token, creating the file with a fresh token when it does not exist yet. The
/// caller has already run [`check_containment`]. Two processes racing here both end up
/// reading the one file that won the `create_new`.
pub fn ensure(path: &Path) -> Result<String, TokenError> {
    match read(path) {
        Err(TokenError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                crate::store::create_state_dir(dir)
                    .map_err(|e| io_err(dir, std::io::Error::other(e.to_string())))?;
            }
            match write_new(path, &mint()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(path, e)),
            }
            read(path)
        }
        other => other,
    }
}

/// Replace the token with a fresh one: write a temp file beside it, then rename it over the
/// old one, so a concurrent reader sees the old token or the new one, never a partial
/// file. Returns the new token.
pub fn rotate(path: &Path) -> Result<String, TokenError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::store::create_state_dir(dir)
        .map_err(|e| io_err(dir, std::io::Error::other(e.to_string())))?;
    let suffix: u64 = rand::random();
    let tmp = dir.join(format!("{TOKEN_FILE}.{suffix:016x}.tmp"));
    let token = mint();
    write_new(&tmp, &token).map_err(|e| io_err(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io_err(path, e))?; // http-token-write: blessed by the HTTP token invariant amendment (AGENTS.md)
    Ok(token)
}

/// Read the current token. Whitespace around it is ignored, so a hand-edited file with a
/// trailing newline still matches.
pub fn read(path: &Path) -> Result<String, TokenError> {
    let raw = std::fs::read_to_string(path).map_err(|e| io_err(path, e))?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(TokenError::Empty(path.display().to_string()));
    }
    Ok(token.to_string())
}

/// Whether an `Authorization` header value carries the current token. The comparison runs
/// in time independent of where the strings differ; only the length can leak, and every
/// minted token has the same length. A token file that cannot be read fails closed.
pub fn authorizes(path: &Path, header: Option<&str>) -> bool {
    let Some(presented) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    let Ok(expected) = read(path) else {
        return false;
    };
    constant_time_eq(presented.trim().as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_once_then_reads_the_same_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kaibo").join(TOKEN_FILE);
        let first = ensure(&path).unwrap();
        assert_eq!(first.len(), 43, "32 bytes, unpadded base64url");
        assert_eq!(ensure(&path).unwrap(), first, "an existing token is kept");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        ensure(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        rotate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a rotated token keeps owner-only permissions");
    }

    #[test]
    fn rotation_replaces_the_token_and_the_old_one_stops_working() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        let old = ensure(&path).unwrap();
        assert!(authorizes(&path, Some(&format!("Bearer {old}"))));
        let new = rotate(&path).unwrap();
        assert_ne!(old, new);
        assert!(!authorizes(&path, Some(&format!("Bearer {old}"))));
        assert!(authorizes(&path, Some(&format!("Bearer {new}"))));
    }

    #[test]
    fn authorization_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        assert!(!authorizes(&path, Some("Bearer anything")), "no token file");
        let token = ensure(&path).unwrap();
        assert!(!authorizes(&path, None), "no header");
        assert!(!authorizes(&path, Some(&token)), "no Bearer scheme");
        assert!(!authorizes(&path, Some(&format!("Basic {token}"))));
        assert!(!authorizes(&path, Some("Bearer ")));
        let mut wrong = token.clone();
        wrong.replace_range(0..1, if token.starts_with('A') { "B" } else { "A" });
        assert!(!authorizes(&path, Some(&format!("Bearer {wrong}"))));
    }

    #[test]
    fn a_token_path_inside_an_allowed_tree_is_refused() {
        let tree = tempfile::tempdir().unwrap();
        let inside = tree.path().join("state").join(TOKEN_FILE);
        assert!(matches!(
            check_containment(&inside, &[tree.path()]),
            Err(TokenError::PathInAllowedTree(_))
        ));
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join(TOKEN_FILE);
        assert!(check_containment(&outside, &[tree.path()]).is_ok());
    }
}
