//! The Unix socket that carries kaibo's streamable HTTP transport.
//!
//! The transport has no network address and no password. Its door is a socket file whose
//! mode is `0600`, so the kernel admits only the user who started kaibo, and the server
//! also checks each accepted connection's peer uid against its own. A browser cannot open
//! a Unix socket, so DNS rebinding and cross-site requests have nothing to reach.
//!
//! The default path is `$XDG_RUNTIME_DIR/kaibo/mcp.sock`, else
//! `$XDG_STATE_HOME/kaibo/mcp.sock` (`~/.local/state/kaibo/mcp.sock`) where there is no
//! runtime dir, as on macOS. The runtime dir is the right home: the system makes it per
//! user, mode `0700`, and empties it at logout, so a stale socket does not outlive a login.
//!
//! The socket file is kaibo's third write surface, under the same discipline as the store
//! and the media CAS: a path no model can aim, refused if it resolves into an allowed
//! tree ([`check_containment`]) or sits in a directory another user could replace
//! ([`refuse_replaceable_dir`]), and created, narrowed, and removed only through
//! individually blessed lines (`tests/no_write_path.rs`, `tests/no_bind.rs`). Kaibo
//! removes only a path that is a socket owned by its own uid: a stale one (nothing
//! accepts on it) before it binds, and its own at shutdown.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The socket's file name inside its directory.
const SOCKET_FILE: &str = "mcp.sock";

/// Why the socket could not be placed or bound.
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    /// The socket path resolves inside an allowed tree, where a model could see it.
    #[error(
        "refused the socket path {0}: it is inside an allowed tree, so the model team could \
         see it. Set `[server.http] socket` (or `--socket`) to a path outside --root and \
         --allow-path, or move the state dir with XDG_STATE_HOME."
    )]
    PathInAllowedTree(String),
    /// Neither `$XDG_RUNTIME_DIR`, `$XDG_STATE_HOME`, nor `$HOME` is set.
    #[error(
        "no place for the socket: XDG_RUNTIME_DIR, XDG_STATE_HOME, and HOME are all unset. \
         Set `[server.http] socket` or pass `--socket PATH`."
    )]
    NoDefault,
    /// The path is taken by something that is not a socket, or by a socket another user owns.
    #[error(
        "refused the socket path {path}: {why}. kaibo replaces only a stale socket it \
         owns. Choose another path with `[server.http] socket`, or remove the file yourself \
         if you put it there."
    )]
    Occupied { path: String, why: &'static str },
    /// Another user could replace the socket's directory or one of its parents, and with it
    /// the socket: a client would then connect to the other user's server.
    #[error(
        "refused the socket directory {dir}: {ancestor} is {why}, so another user could \
         replace the socket kaibo's clients connect to. Set `[server.http] socket` to a \
         path under a directory only you and root control, such as $XDG_RUNTIME_DIR."
    )]
    ReplaceableDir {
        dir: String,
        ancestor: String,
        why: String,
    },
    /// The socket at the path changed between kaibo's check and its removal.
    #[error(
        "the socket at {0} changed while kaibo was checking it, so another process is \
         using the path. Try again, or give this server its own path with `[server.http] \
         socket`."
    )]
    Replaced(String),
    /// A kaibo (or another server) is already accepting on the path.
    #[error(
        "another server already accepts connections on {0}. Stop it first, or give this \
         one its own path with `[server.http] socket` or `--socket PATH`."
    )]
    InUse(String),
    /// The path is longer than a socket address holds.
    #[error(
        "the socket path {path} is {len} bytes; the system allows at most {max}. Set \
         `[server.http] socket` to a shorter path, for example one under $XDG_RUNTIME_DIR."
    )]
    TooLong {
        path: String,
        len: usize,
        max: usize,
    },
    /// Any filesystem failure, with the path it hit.
    #[error("socket {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> SocketError {
    SocketError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The default socket path, from an environment lookup. Relative values are ignored, as the
/// XDG spec says.
pub fn default_socket_path_with(
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, SocketError> {
    let absolute = |name: &str| get(name).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(run) = absolute("XDG_RUNTIME_DIR") {
        return Ok(run.join("kaibo").join(SOCKET_FILE));
    }
    if let Some(state) = absolute("XDG_STATE_HOME") {
        return Ok(state.join("kaibo").join(SOCKET_FILE));
    }
    absolute("HOME")
        .map(|h| h.join(".local/state/kaibo").join(SOCKET_FILE))
        .ok_or(SocketError::NoDefault)
}

/// The default socket path for this process's environment.
pub fn default_socket_path() -> Result<PathBuf, SocketError> {
    default_socket_path_with(|k| std::env::var_os(k))
}

/// Refuse a socket path that resolves into any tree in `allowed_trees` (`--root` and
/// `--allow-path`): a model that can read there could see the socket exist, and a
/// model-steered path would be a write aimed by the model. A symlinked parent is followed
/// before the comparison. A followed git worktree is not in that set, so a socket path in
/// one passes; the socket stays owner-only either way.
pub fn check_containment(path: &Path, allowed_trees: &[&Path]) -> Result<(), SocketError> {
    let name = path.file_name().unwrap_or_default();
    let resolved = crate::store::resolve_existing_parent(path).join(name);
    for tree in allowed_trees {
        let tree = tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf());
        if resolved.starts_with(&tree) {
            return Err(SocketError::PathInAllowedTree(path.display().to_string()));
        }
    }
    Ok(())
}

/// Our effective uid: the owner of every socket kaibo creates.
fn my_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A file's identity: device and inode.
type FileId = (u64, u64);

fn file_id(meta: &std::fs::Metadata) -> FileId {
    (meta.dev(), meta.ino())
}

/// Remove a socket that is ours. Every removal in kaibo goes through here, so no caller can
/// aim it at a file that is not a socket owned by this uid. With `expect`, the file must
/// also be the very one the caller saw, so a socket another server bound at the same path
/// since then is left alone.
fn remove_own_socket(path: &Path, expect: Option<FileId>) -> Result<(), SocketError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| io_err(path, e))?;
    if !meta.file_type().is_socket() {
        return Err(SocketError::Occupied {
            path: path.display().to_string(),
            why: "it is not a socket",
        });
    }
    if meta.uid() != my_uid() {
        return Err(SocketError::Occupied {
            path: path.display().to_string(),
            why: "another user owns it",
        });
    }
    if expect.is_some_and(|id| id != file_id(&meta)) {
        return Err(SocketError::Replaced(path.display().to_string()));
    }
    let removed = std::fs::remove_file(path); // http-socket-remove: blessed by the HTTP socket invariant amendment (AGENTS.md)
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_err(path, e)),
        _ => Ok(()),
    }
}

/// Refuse a socket directory that another user could replace, by the credentials store's
/// rule (OpenSSH's StrictModes): the directory and each ancestor must be owned by this
/// user or root, and not writable by group or others unless sticky (as `/tmp` is). Without
/// this, a user who can write a parent unlinks the socket and binds their own at its
/// path, and the user's agent then talks to that server. `resolved` must be canonical.
fn refuse_replaceable_dir(resolved: &Path) -> Result<(), SocketError> {
    let me = my_uid();
    for ancestor in resolved.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor).map_err(|e| io_err(ancestor, e))?;
        let mode = meta.mode();
        let sticky = mode & 0o1000 != 0;
        let owner_ok = meta.uid() == me || meta.uid() == 0;
        let open_to_others = mode & 0o022 != 0 && !sticky;
        if meta.file_type().is_symlink() || !owner_ok || open_to_others {
            let why = if meta.file_type().is_symlink() {
                "a symlink".to_string()
            } else if !owner_ok {
                format!("owned by another user (uid {})", meta.uid())
            } else {
                format!("writable by other users (mode {:o})", mode & 0o7777)
            };
            return Err(SocketError::ReplaceableDir {
                dir: resolved.display().to_string(),
                ancestor: ancestor.display().to_string(),
                why,
            });
        }
    }
    Ok(())
}

/// A bound socket. Dropping it removes the socket file, so a clean shutdown leaves nothing
/// behind; a crash leaves a stale file, which the next start replaces.
pub struct BoundSocket {
    pub listener: tokio::net::UnixListener,
    path: PathBuf,
    /// The file this server bound, so shutdown never removes another server's socket.
    id: FileId,
}

impl BoundSocket {
    /// The uid every accepted connection must come from: ours.
    pub fn owner_uid(&self) -> u32 {
        my_uid()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        if let Err(e) = remove_own_socket(&self.path, Some(self.id)) {
            tracing::warn!(error = %e, "could not remove the socket file at shutdown");
        }
    }
}

/// Bind `path` and narrow it to the owner. Creates the parent directory, replaces a stale
/// socket this user owns, and refuses a live one or anything else in the way. The caller
/// has already run [`check_containment`]. Must run inside a tokio runtime.
pub fn bind(path: &Path) -> Result<BoundSocket, SocketError> {
    // sockaddr_un.sun_path holds 108 bytes on Linux and 104 on macOS, with a trailing NUL.
    let max = if cfg!(target_os = "linux") { 107 } else { 103 };
    let len = path.as_os_str().len();
    if len > max {
        return Err(SocketError::TooLong {
            path: path.display().to_string(),
            len,
            max,
        });
    }
    let name = path.file_name().ok_or_else(|| SocketError::Occupied {
        path: path.display().to_string(),
        why: "it names no file",
    })?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.ok_or_else(|| SocketError::Occupied {
        path: path.display().to_string(),
        why: "it has no parent directory",
    })?;
    crate::store::create_state_dir(dir)
        .map_err(|e| io_err(dir, std::io::Error::other(e.to_string())))?;
    // Check, then use only the canonical path the check walked.
    let dir = dir.canonicalize().map_err(|e| io_err(dir, e))?;
    refuse_replaceable_dir(&dir)?;
    let path = dir.join(name);
    let path = path.as_path();
    match std::fs::symlink_metadata(path) {
        Ok(seen) => {
            // Something is there. A live server answers; anything that refuses is stale,
            // and the removal insists it is still the file we looked at.
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Err(SocketError::InUse(path.display().to_string())),
                Err(_) => remove_own_socket(path, Some(file_id(&seen)))?,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(path, e)),
    }
    let listener = tokio::net::UnixListener::bind(path) // loopback-bind: the opt-in MCP Unix socket (AGENTS.md)
        .map_err(|e| io_err(path, e))?;
    let id = file_id(&std::fs::symlink_metadata(path).map_err(|e| io_err(path, e))?);
    let bound = BoundSocket {
        listener,
        path: path.to_path_buf(),
        id,
    };
    // The bind made the file under the process umask; the owner-only mode closes the gap.
    // Until then the peer-uid check in the accept loop is the guard.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) // http-socket-mode: blessed by the HTTP socket invariant amendment (AGENTS.md)
        .map_err(|e| io_err(path, e))?;
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    }

    #[test]
    fn the_runtime_dir_wins_then_state_then_home() {
        let p = default_socket_path_with(env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_STATE_HOME", "/s"),
            ("HOME", "/h"),
        ]))
        .unwrap();
        assert_eq!(p, Path::new("/run/user/1000/kaibo/mcp.sock"));
        let p = default_socket_path_with(env(&[("XDG_STATE_HOME", "/s"), ("HOME", "/h")])).unwrap();
        assert_eq!(p, Path::new("/s/kaibo/mcp.sock"));
        let p = default_socket_path_with(env(&[("HOME", "/h")])).unwrap();
        assert_eq!(p, Path::new("/h/.local/state/kaibo/mcp.sock"));
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        let p =
            default_socket_path_with(env(&[("XDG_RUNTIME_DIR", "run"), ("HOME", "/h")])).unwrap();
        assert_eq!(p, Path::new("/h/.local/state/kaibo/mcp.sock"));
        assert!(matches!(
            default_socket_path_with(env(&[])),
            Err(SocketError::NoDefault)
        ));
    }

    #[test]
    fn a_path_inside_an_allowed_tree_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("kaibo").join("mcp.sock");
        let err = check_containment(&inside, &[dir.path()]).unwrap_err();
        assert!(matches!(err, SocketError::PathInAllowedTree(_)), "{err}");
        let other = tempfile::tempdir().unwrap();
        assert!(check_containment(&inside, &[other.path()]).is_ok());
    }

    #[tokio::test]
    async fn bind_narrows_the_mode_and_drop_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k").join("mcp.sock");
        let bound = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the socket is owner-only");
        drop(bound);
        assert!(!path.exists(), "a clean shutdown leaves no socket behind");
    }

    #[tokio::test]
    async fn a_live_socket_is_refused_and_a_stale_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let live = bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(SocketError::InUse(_))));
        drop(live);
        // Simulate a crash: a listener closes but its file stays.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the crash left the file");
        let again = bind(&path).expect("a stale socket is replaced");
        drop(again);
    }

    #[tokio::test]
    async fn a_regular_file_in_the_way_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        std::fs::write(&path, "precious").unwrap();
        let err = bind(&path).err().expect("refused");
        assert!(matches!(err, SocketError::Occupied { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    }

    #[tokio::test]
    async fn an_overlong_path_is_refused_with_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x".repeat(120)).join("mcp.sock");
        let err = bind(&path).err().expect("refused");
        assert!(matches!(err, SocketError::TooLong { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_directory_other_users_can_write_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = bind(&open.join("mcp.sock")).err().expect("refused");
        assert!(matches!(err, SocketError::ReplaceableDir { .. }), "{err}");
        assert!(!open.join("mcp.sock").exists(), "nothing was bound");
    }

    /// Shutdown removes the socket this server bound, and only that one: a socket another
    /// server bound at the same path since then stays.
    #[tokio::test]
    async fn shutdown_never_removes_another_servers_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let first = bind(&path).unwrap();
        // Another server takes the path over: the file is replaced by a different one.
        std::fs::remove_file(&path).unwrap();
        let second = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(first);
        assert!(
            path.exists(),
            "the second server's socket survives the first one's shutdown"
        );
        drop(second);
    }

    #[test]
    fn removal_insists_on_the_file_it_saw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let _live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let wrong = (0, 0);
        let err = remove_own_socket(&path, Some(wrong)).unwrap_err();
        assert!(matches!(err, SocketError::Replaced(_)), "{err}");
        assert!(path.exists());
    }
}
