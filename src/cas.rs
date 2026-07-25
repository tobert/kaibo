//! Content-addressed store for generated media artifacts.
//!
//! This is the second and last deliberate write surface kaibo has (the first is
//! [`crate::store`]'s XDG state db). Where that store earns its exception through
//! *policy* (a containment check plus a single blessed `create_dir_all`), the CAS earns
//! it through **shape**: the address of every object is its own SHA-256 digest, so
//! there is no destination-path parameter anywhere in this module's public API for a
//! caller to aim. A model can hand kaibo bytes and get back a digest; it can never tell
//! kaibo *where* to put them. That is the whole safety argument — everything else here
//! (containment, `create_new`, the soft cap) exists to keep that shape honest under
//! adversarial input, not to substitute for it.
//!
//! # Why `$XDG_DATA_HOME`, not `$XDG_STATE_HOME`
//!
//! [`crate::store`] lives under the *state* dir — Amy's stance there is the db is
//! reconstructible-or-disposable operator bookkeeping. Generated images are different:
//! they are artifacts the user *paid for* (tokens, provider dollars, sometimes a seed
//! nobody kept), and may never be reproducible again. That is squarely what the XDG
//! *data* dir is for — durable user data a backup tool (`rsync`, `restic`) should carry.
//! Putting the CAS in state right next to a "delete me on corruption" db would invite
//! exactly the wrong operator instinct. See `docs/media-cas.md` for the full reasoning.
//!
//! # The four safety properties, and how each is enforced
//!
//! - **Containment** ([`Cas::open`]): refuses to open at a path that resolves inside any
//!   allowed project tree, mirroring [`crate::store::SessionStore::open`] byte-for-byte
//!   in spirit — canonicalize the deepest *existing* ancestor (so a not-yet-created dir
//!   can still be checked, and a symlink into a project can't be used to sneak past a
//!   lexical compare), and refuse *before* any side effect.
//! - **The [`Digest`] newtype**: a digest arriving from outside kaibo (a provider's
//!   response id, a client's `get` argument) is validated — exactly 64 lowercase hex
//!   characters — before it is ever allowed to become part of a path. An unvalidated
//!   string handed straight to `format!("{s}.png")` is a path-traversal vector
//!   (`../../etc/passwd` fails length+charset trivially, but so would anything less
//!   obviously hostile); [`Digest::from_hex`] makes that class of bug structurally
//!   impossible rather than something every call site has to remember to check.
//! - **[`Extension`] is an enum**, never a caller string. It is the *only*
//!   caller-influenced component of the on-disk path (the digest supplies the rest). A
//!   free-form string here would reopen exactly the aim-the-write hole the digest
//!   already closed for the rest of the path — so it doesn't get one.
//! - **Write-only** ([`Cas::put`]): every file is created with `create_new` (`O_EXCL` on
//!   Unix). There is no unlink, truncate, or rename anywhere in this module. Because the
//!   path is the content's own hash, `AlreadyExists` cannot mean "someone else's data is
//!   here" — it can only mean "these exact bytes are already here" — so it is mapped to
//!   `Ok`, not an error and not a rewrite. Editing is copy-on-write for free: different
//!   bytes hash to a different address, so an "edit" is just a new object.
//!
//! # The soft cap refuses; it never evicts
//!
//! [`Cas::open`] takes a `max_bytes` ceiling. A [`Cas::put`] that would push the store's
//! total size over that ceiling is refused with [`CasError::CapacityExceeded`] — loudly,
//! before any bytes are written. It never deletes anything to make room: eviction would
//! quietly make "write-only" a lie (a lie exactly one write-time trust judgment away from
//! looking corrupt), and disk-full-in-effect is precisely the crash-over-corrupt case
//! kaibo's read-only posture already treats as correct. A dedup write of content already
//! on disk is exempt from the check (it adds zero bytes, so it cannot be what pushed the
//! store over the line) — see [`Cas::put`].
//!
//! # No GC here, on purpose — the sidecar is why that's honest
//!
//! kaibo does not prune this store. The user backs it up or prunes it by hand (`find
//! -mtime`, `restic`, …) if they want to. For that to be an honest position rather than a
//! punt, every object gets a `<hex>.json` provenance sidecar (prompt, model, cast,
//! timestamp, mime) next to it — the metadata a human (or a script) needs to prune
//! intelligently instead of guessing at opaque bytes.
//!
//! # Never mounted into kaish
//!
//! This store is never exposed to the model team's read-only shell. Read access alone
//! wouldn't touch the write-only claim, but it *would* breach the operator/model-team
//! line (`AGENTS.md`): kaibo state spans projects, and a browsable CAS mount would let
//! any project's model team enumerate every other project's generated artifacts. The
//! address is the capability here — you can only read a digest you already have — so the
//! one leak surface a mount would open is enumeration, not disclosure of an already-known
//! object. See `docs/media-cas.md`'s "Decided: no CAS access from kaish".
//!
//! # `tests/no_write_path.rs`
//!
//! Three call sites here carry the module's blessed marker on their own line: the
//! shard-directory `create_dir_all` in [`Cas::put`], the `create_new` open, and the
//! `write_all` of the bytes. That guard enumerates them individually — see its module
//! doc for why one write-only store needs more than one blessed needle.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The CAS's error surface. A persistence-adjacent module wants a typed error callers
/// can match on, mirroring [`crate::store::StoreError`]'s posture (thiserror over
/// kaibo's usual anyhow-everywhere interior).
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    /// The requested CAS root resolves inside an allowed project tree — refused so the
    /// store can never be pointed where a model can reach it, mirroring
    /// [`crate::store::StoreError::PathInAllowedTree`].
    #[error("media CAS path must live outside every allowed project tree, but {0} is inside one")]
    PathInAllowedTree(String),
    /// A digest string failed validation before it could touch a path: not exactly 64
    /// lowercase hex characters. This is the structural guard against path traversal —
    /// see the module doc.
    #[error("digest must be exactly 64 lowercase hex characters, got {0:?}")]
    InvalidDigest(String),
    /// A write would push the store's total size past its configured soft cap. Refused
    /// loudly, nothing evicted — see the module doc's "The soft cap refuses" section.
    #[error(
        "media CAS write refused: the soft cap of {max_bytes} bytes would be exceeded (current \
         usage {current_bytes} bytes, kaibo never evicts to make room — raise the cap or prune \
         the CAS directory by hand)"
    )]
    CapacityExceeded { max_bytes: u64, current_bytes: u64 },
    /// A filesystem operation failed for a reason other than the ones above (permissions,
    /// disk full, an unreadable existing file, …), surfaced verbatim.
    #[error("media CAS io: {0}")]
    Io(String),
    /// The provenance sidecar failed to serialize to JSON. `Provenance`'s fields are
    /// plain strings/ints, so this should not happen in practice; surfaced loudly rather
    /// than silently dropping the sidecar (which would defeat the whole GC-by-sidecar
    /// story — see the module doc).
    #[error("media CAS provenance sidecar serialize: {0}")]
    Serialize(String),
}

pub type Result<T> = std::result::Result<T, CasError>;

/// A validated SHA-256 digest — the address of one object in the store.
///
/// This is the load-bearing type of the whole module: constructing one is the *only*
/// way a hex string becomes eligible to appear in a filesystem path here, and
/// [`Digest::from_hex`] is the *only* way to construct one from an untrusted string
/// ([`Digest::of_bytes`] derives one directly from content, which is always safe — a
/// hash of arbitrary bytes cannot itself be a traversal string once run through the
/// hex encoder). There is no `pub` way to build a `Digest` that skips validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Validate `s` as a digest: exactly 64 lowercase ASCII hex characters, nothing
    /// else. Rejects uppercase (a provider or client must match the canonical lowercase
    /// form byte-for-byte — silently folding case would let two textually different
    /// strings address the same object, a footgun this store doesn't need), rejects any
    /// wrong length, and rejects any non-hex byte. A path-traversal-shaped string
    /// (`../../etc/passwd`) is refused by the length/charset check alone — it never gets
    /// close to being interpreted as a path component.
    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CasError::InvalidDigest(s.to_string()));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            // Bounds and charset already checked above; from_str_radix cannot fail here,
            // but we still surface a typed error instead of unwrapping in a production path.
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| CasError::InvalidDigest(s.to_string()))?;
        }
        Ok(Digest(out))
    }

    /// Hash `bytes` with SHA-256 and wrap the result as a `Digest`. Always succeeds —
    /// there is no "invalid content" for a hash function, only invalid *hex strings*
    /// claiming to already be one (see [`Digest::from_hex`]).
    pub fn of_bytes(bytes: &[u8]) -> Self {
        use sha2::Digest as _; // the sha2 trait, shadowed locally; our own `Digest` wins elsewhere.
        let hash = sha2::Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&hash);
        Digest(out)
    }

    /// The canonical lowercase hex form — what actually appears in file names.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A generated image's on-disk container format. Deliberately a closed enum, not a
/// caller-supplied string: this is the *only* caller-influenced component of an
/// object's path (the digest supplies the rest), so it must be a small, fixed set kaibo
/// controls — a free string here would reopen the aim-the-write hole the digest closes
/// for the rest of the path. Extend this list as new providers/formats land; never
/// widen it to `String`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extension {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl Extension {
    /// Every variant, in the order [`Cas::path_for`]/[`Cas::get`] probe them — a digest
    /// alone doesn't say which format it was written under, so a lookup by digest tries
    /// each in turn until one exists on disk.
    pub const ALL: [Extension; 4] = [
        Extension::Png,
        Extension::Jpeg,
        Extension::Webp,
        Extension::Gif,
    ];

    /// The bare filename extension (no leading dot).
    pub fn as_str(&self) -> &'static str {
        match self {
            Extension::Png => "png",
            Extension::Jpeg => "jpeg",
            Extension::Webp => "webp",
            Extension::Gif => "gif",
        }
    }
}

/// The provenance sidecar recorded next to every object: `prompt`, `model`, `cast`,
/// `timestamp` (epoch seconds), `mime`. This is the whole point of shipping with no
/// built-in GC — a user (or their own script) can `find`/`jq` over these sidecars and
/// prune intelligently instead of guessing at opaque hashed bytes. See the module doc's
/// "No GC here, on purpose" section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub prompt: String,
    pub model: String,
    pub cast: String,
    pub timestamp: i64,
    pub mime: String,
}

/// A content-addressed store of generated media artifacts, rooted at a fixed,
/// model-inaccessible directory. See the module doc for the full safety argument;
/// briefly: the address is the SHA-256 of the content, so no `pub` method here takes a
/// destination path, and every write is `create_new` (never a truncate/overwrite).
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
    max_bytes: u64,
}

impl Cas {
    /// Open (but do not yet create — see below) the CAS rooted at `dir`, refusing if
    /// `dir` resolves inside any `allowed_trees` entry (the read-only sandbox's project
    /// roots). `max_bytes` is the soft cap enforced by every [`Cas::put`].
    ///
    /// Unlike [`crate::store::SessionStore::open`], this does **not** create any
    /// directory — there is nothing to eagerly create. The root (and every shard
    /// directory under it) comes into existence lazily, the first time [`Cas::put`]
    /// needs it, via one `create_dir_all` call that creates every missing ancestor at
    /// once (root included). That keeps `open` itself a pure containment check with zero
    /// filesystem side effects — a path that fails containment never touches disk at
    /// all, and a `Cas` that is opened but never written to leaves no trace.
    ///
    /// The containment check mirrors [`crate::store::SessionStore::open`]: absolutize a
    /// relative `dir` against the current directory first (so a relative path whose
    /// parent doesn't exist yet can't slip past a lexical-only compare via cwd), then
    /// canonicalize the deepest *existing* ancestor of `dir` (following symlinks) and
    /// re-append the not-yet-created tail lexically, and refuse if that resolves inside
    /// any allowed tree.
    pub fn open(dir: &Path, allowed_trees: &[&Path], max_bytes: u64) -> Result<Self> {
        let dir = absolutize(dir)?;
        let resolved = resolve_existing_ancestor(&dir);
        for tree in allowed_trees {
            let tree_resolved = tree.canonicalize().unwrap_or_else(|_| normalize(tree));
            if resolved.starts_with(&tree_resolved) {
                return Err(CasError::PathInAllowedTree(dir.display().to_string()));
            }
        }
        Ok(Self {
            root: dir,
            max_bytes,
        })
    }

    /// The CAS root directory this store was opened with.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The two-level hex shard directory for `digest`: `<hex[0..2]>/<hex[2..4]>/` under
    /// the root. Two levels keeps any one directory's entry count small even at large
    /// object counts (256 * 256 shards), without needing a config knob for it.
    fn shard_dir(&self, digest: &Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join(&hex[0..2]).join(&hex[2..4])
    }

    fn object_path(&self, digest: &Digest, ext: Extension) -> PathBuf {
        self.shard_dir(digest)
            .join(format!("{}.{}", digest.to_hex(), ext.as_str()))
    }

    fn sidecar_path(&self, digest: &Digest) -> PathBuf {
        self.shard_dir(digest)
            .join(format!("{}.json", digest.to_hex()))
    }

    /// The path an object would live at, if it exists — probing every [`Extension`]
    /// variant in turn, since a bare digest doesn't say which format it was written
    /// under. `None` if no object with this digest has ever been written (or an ancestor
    /// directory doesn't exist yet, which reads the same way: nothing is here).
    pub fn path_for(&self, digest: &Digest) -> Option<PathBuf> {
        Extension::ALL.into_iter().find_map(|ext| {
            let path = self.object_path(digest, ext);
            path.is_file().then_some(path)
        })
    }

    /// Read an object's bytes back by digest, or `None` if nothing with this digest has
    /// been written. A read failure on a path that *does* exist (permissions, a
    /// mid-write crash leaving a partial file some other tool created out-of-band) is
    /// folded into `None` here — the read side has no destination-path parameter to
    /// misuse and no obligation to distinguish "missing" from "unreadable" for a caller
    /// that only wants bytes; a caller that needs the distinction can stat
    /// [`Cas::path_for`] itself.
    pub fn get(&self, digest: &Digest) -> Option<Vec<u8>> {
        let path = self.path_for(digest)?;
        std::fs::read(path).ok()
    }

    /// Recursively sum the size in bytes of every file currently in the store. Backs the
    /// soft-cap check in [`Cas::put`]. A read-only directory walk — no writes, no
    /// caching, no separate counter file to keep in sync (and thus no extra write site
    /// this module would otherwise need).
    fn total_bytes(&self) -> Result<u64> {
        fn walk(dir: &Path) -> std::io::Result<u64> {
            if !dir.is_dir() {
                return Ok(0);
            }
            let mut total = 0u64;
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    total += walk(&path)?;
                } else {
                    total += entry.metadata()?.len();
                }
            }
            Ok(total)
        }
        walk(&self.root).map_err(|e| CasError::Io(format!("scanning CAS size: {e}")))
    }

    /// Write `bytes` (a generated artifact of container format `ext`) into the store,
    /// with `provenance` recorded in the `<hex>.json` sidecar, and return its digest.
    ///
    /// The digest is computed from `bytes` — the caller supplies no path, and cannot
    /// influence where the object lands beyond choosing `ext` (a closed enum). If an
    /// object with this exact digest already exists, this is a no-op: same hash means
    /// same bytes, so nothing is rewritten and the existing digest is returned as-is
    /// (**not** re-checked against the soft cap — it adds zero new bytes, so it cannot be
    /// what pushes the store over the line). Otherwise the soft cap is checked *before*
    /// any write: if the current total plus `bytes.len()` would exceed `max_bytes`, the
    /// write is refused with [`CasError::CapacityExceeded`] and nothing is written —
    /// never evicted, see the module doc.
    pub fn put(&self, bytes: &[u8], ext: Extension, provenance: &Provenance) -> Result<Digest> {
        let digest = Digest::of_bytes(bytes);
        let shard = self.shard_dir(&digest);
        let obj_path = self.object_path(&digest, ext);
        let sidecar_path = self.sidecar_path(&digest);

        if !obj_path.is_file() {
            // New content only: check the soft cap before growing the store at all.
            let current = self.total_bytes()?;
            let incoming = bytes.len() as u64;
            if current.saturating_add(incoming) > self.max_bytes {
                return Err(CasError::CapacityExceeded {
                    max_bytes: self.max_bytes,
                    current_bytes: current,
                });
            }
        }

        std::fs::create_dir_all(&shard) // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
            .map_err(|e| {
                CasError::Io(format!("creating cas shard dir {}: {e}", shard.display()))
            })?;

        write_new_file(&obj_path, bytes)
            .map_err(|e| CasError::Io(format!("writing cas object {}: {e}", obj_path.display())))?;

        let sidecar_json = serde_json::to_vec_pretty(provenance)
            .map_err(|e| CasError::Serialize(e.to_string()))?;
        write_new_file(&sidecar_path, &sidecar_json).map_err(|e| {
            CasError::Io(format!(
                "writing cas provenance sidecar {}: {e}",
                sidecar_path.display()
            ))
        })?;

        Ok(digest)
    }
}

/// Create `path` as a brand-new file (`create_new` — `O_EXCL` on Unix) and write `bytes`
/// to it. If `path` already exists this is a deliberate no-op (`Ok(())`, not an error):
/// on a content-addressed path that can only mean these exact bytes are already there,
/// so there is nothing to rewrite. Any other I/O failure (permissions, disk full, the
/// parent directory missing) is propagated. There is no unlink, truncate, or rename
/// anywhere in this function or its caller — an "edit" is always a new digest, never a
/// mutation of an existing object.
fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(path) // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    };
    file.write_all(bytes) // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
}

/// The default media CAS directory: `$XDG_DATA_HOME/kaibo/cas`, else
/// `~/.local/share/kaibo/cas` — the XDG *data* dir, deliberately not the *state* dir
/// [`crate::store`] uses (see the module doc's "Why `$XDG_DATA_HOME`" section). `None`
/// if neither `$XDG_DATA_HOME` nor `$HOME` is set.
pub fn default_cas_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("kaibo").join("cas"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("kaibo")
            .join("cas")
    })
}

/// Absolutize `dir` against the current directory if it's relative, so the containment
/// compare in [`Cas::open`] only ever handles absolute paths. Mirrors
/// `SessionStore::open`'s handling of a relative state-db path (Gemini review finding 1
/// there: a lexical-only fallback that stayed relative let `starts_with` trivially miss
/// containment via cwd).
fn absolutize(dir: &Path) -> Result<PathBuf> {
    if dir.is_absolute() {
        return Ok(dir.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| {
        CasError::Io(format!(
            "{}: cannot resolve current dir to absolutize a relative CAS path: {e}",
            dir.display()
        ))
    })?;
    Ok(cwd.join(dir))
}

/// Resolve `path` to a canonical, absolute form for the containment compare, without
/// requiring `path` to exist. Canonicalizes the deepest *existing* ancestor (following
/// symlinks there) and re-appends the not-yet-created tail lexically — mirrors
/// `store.rs`'s `resolve_existing_parent`, applied to the CAS root itself rather than a
/// db file's parent (there is no filename component to split off here: the CAS root is
/// a directory, not a file).
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor: &Path = path;
    loop {
        if let Ok(canon) = cursor.canonicalize() {
            let mut base = canon;
            for name in tail.iter().rev() {
                base.push(name);
            }
            return normalize(&base);
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(up)) => {
                tail.push(name.to_owned());
                cursor = up;
            }
            _ => break,
        }
    }
    normalize(path)
}

/// Lexically clean a path (resolve `.` and `..` without touching the filesystem).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
