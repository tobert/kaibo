//! Content-addressed store for generated media artifacts.
//!
//! This is the second deliberate write surface kaibo has (the first is
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
//! exactly the wrong operator instinct.
//!
//! # Two alternatives that were considered and rejected
//!
//! Recorded here rather than in a design doc because this is where someone would think of
//! them — both look obviously better until you check.
//!
//! **Blobs in the turso db, instead of files.** kaibo already has a SQLite store, so
//! putting artifacts in it looks like one fewer moving part. Checked turso 0.7.0 directly:
//! `Value::Blob(Vec<u8>)` is the *entire* surface — there is no incremental or streaming
//! blob I/O — so every read and write materializes a whole multi-megabyte image in memory.
//! The bytes would land in overflow page chains that grow the db file with no working
//! VACUUM story, and they would live inside the one file whose documented lethal hazard is
//! *silently losing acknowledged writes* on a mixed MP/non-MP open (see
//! [`crate::store`]). It would also destroy the `rsync`/`find`/`restic` story that makes
//! the no-GC stance honest — a user cannot prune blobs in a SQLite file with `find`.
//! Files win on every axis.
//!
//! **blake3 instead of sha2.** Faster on paper, and the usual choice for a modern CAS.
//! But `sha2` is *already in our dependency tree* (pure Rust, no C), while `blake3` is not
//! and carries a `cc` build-dependency; its `pure` feature disables the C/asm path, but
//! that is still a new dependency and a build-shape decision. The speed edge it buys is
//! invisible at image sizes — roughly a millisecond either way, and both are 256-bit. The
//! static-musl/no-C-toolchain invariant (see `AGENTS.md`) costs more to defend than the
//! milliseconds are worth.
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
//!   Unix), then `sync_all`'d before the call returns. There is no unlink, truncate, or
//!   rename anywhere in this module. Because the path is the content's own hash,
//!   `AlreadyExists` *should* mean "these exact bytes are already here" rather than
//!   "someone else's data is here" — but that claim only holds if every object at that
//!   path was placed by this CAS, and placed *completely*. Neither half is structurally
//!   enforced: a crash between `create_new` and a completed `write_all` (the `sync_all`
//!   narrows this window, it cannot close it — see [`write_new_file`]) can leave a
//!   truncated file sitting at the exact path future writes of the same content will see
//!   as "already there." So `AlreadyExists` is no longer trusted blindly — see "Integrity"
//!   below. Editing is copy-on-write for free either way: different bytes hash to a
//!   different address, so an "edit" is just a new object.
//!
//! # Integrity: verify before trusting a path exists — a whole-object guarantee
//!
//! [`Cas::get`] recomputes the SHA-256 of every byte it reads and compares it to the
//! digest the caller asked for *before returning anything*: `Ok(Some(bytes))` means found
//! **and verified**, `Ok(None)` means never written, `Err(CasError::Corrupt)` means an
//! object exists at this address but its content doesn't hash to it (unrecoverable in
//! place — this store never rewrites, so the fix is operator intervention: remove the bad
//! file, re-`put` the content), and `Err(CasError::Io)` means it exists but couldn't even
//! be read. None of these fold into each other — an unreadable object is not "missing," and
//! neither is a hash mismatch; both used to be silently swallowed into `None`, which is
//! exactly the silent-fallback shape this codebase forbids. [`Cas::put`]'s dedup path gets
//! the same treatment: on `AlreadyExists` it reads the existing object back and verifies it
//! before reporting success, rather than trusting the path and silently discarding the
//! caller's good bytes into a slot that was actually poisoned.
//!
//! **Why this is possible here, and why it wouldn't survive streaming reads.** This module
//! only ever reads a whole object into memory (`std::fs::read`) before handing bytes back —
//! so the *entire* content is available to hash before a single byte reaches the caller.
//! That ordering is what makes "verify, then return" a real guarantee rather than
//! after-the-fact bookkeeping. A future streaming read (open a handle, hand the caller a
//! `Read`/`AsyncRead` instead of a `Vec<u8>`) could not inherit this guarantee for free: by
//! the time a stream reached EOF and discovered a hash mismatch, it would already have
//! emitted unverified bytes to whatever was consuming it. Adding streaming reads means
//! *solving* that problem — failing the stream mid-read on mismatch, and requiring every
//! caller to handle a truncated/failed read as a first-class outcome — not carrying this
//! module's verify-before-return contract over unexamined. Read this before adding one.
//!
//! # The soft cap is opt-in; when set it refuses, and it never evicts
//!
//! [`Cas::open`] takes an **optional** `max_bytes` ceiling, and the default is `None` —
//! no ceiling, and no size accounting whatsoever. Enforcing a ceiling means summing every
//! file in the store on the *write* path, for every new object, over a store that never
//! deletes and is sharded two levels deep; that walk is O(objects) and only slows down as
//! the store fills. An operator who never asked for a ceiling should not pay it, so they
//! do not. Cleanup, if an operator wants any, is theirs to do — kaibo builds no index and
//! offers no scan verb for it, on purpose (see "No GC here" below).
//!
//! When a cap *is* set, a [`Cas::put`] that would push the store's total size over it is
//! refused with [`CasError::CapacityExceeded`] — loudly, before any bytes are written. It never deletes anything to make room: eviction would
//! quietly make "write-only" a lie (a lie exactly one write-time trust judgment away from
//! looking corrupt), and disk-full-in-effect is precisely the crash-over-corrupt case
//! kaibo's read-only posture already treats as correct. The check runs on **every** put,
//! including one whose content is already here: exempting a dedup is right about disk
//! usage and wrong about disclosure, because succeeding for held bytes while refusing new
//! ones lets a caller ask *is this content in the store?* across every project this kaibo
//! has served. See [`Cas::put`] for the full argument, and [`MemoryCas::put`] for the same
//! ordering in the other mode.
//!
//! # No GC here, on purpose
//!
//! kaibo does not prune this store, and the reason is the store's own shape rather than a
//! deferred feature: the address is the content hash and nothing is ever unlinked, so an
//! object at a given digest is the same object forever and no reference anyone holds can
//! go stale. Deletion would trade that guarantee away.
//!
//! Every object also gets a `<hex>.json` provenance sidecar (prompt, model, cast,
//! timestamp, mime, seed, and how it was produced) beside it, which makes an object
//! **self-describing to whoever holds its address**: one lookup by digest says what these
//! bytes are, where they came from, and what format to serve them as. That is its job,
//! and it is the access pattern the whole store is built for.
//!
//! It is not a scan target, and stays that way on purpose (Amy, 2026-08-05: no index,
//! ever). Sweeping the tree to survey the store gets slower as the store fills — 65,536
//! shards, a `readdir` and a parse per object — and kaibo builds nothing here to make that
//! walk cheap, because it never runs one: the store stays opaque, reached only by the
//! address a caller already holds. Tracking of what was created lives WITH the
//! conversation that created it, not in the store — a `consult`/`oneshot` session turn
//! already records the artifact footer (digest, mime, size), so those digests sit beside
//! the conversation in the persistence store, not in a CAS-side index. An operator's
//! cleanup, if wanted, is plain file mtime on the object tree for now — a size- or
//! age-based reclaim policy is still open.
//!
//! Because nothing here is ever rewritten, the sidecar's schema can only ever grow
//! *compatibly* — see [`Provenance`]'s doc for the rule that keeps a decade-old sidecar
//! readable.
//!
//! # Never mounted into kaish
//!
//! This store is never exposed to the model team's read-only shell. Read access alone
//! wouldn't touch the write-only claim, but it *would* breach the operator/model-team
//! line (`AGENTS.md`): kaibo state spans projects, and a browsable CAS mount would let
//! any project's model team enumerate every other project's generated artifacts. The
//! address is the capability here — you can only read a digest you already have — so the
//! one leak surface a mount would open is enumeration, not disclosure of an already-known
//! object. Not mounting it is a deliberate decision, not an oversight.
//!
//! # `tests/no_write_path.rs`
//!
//! Three call sites here carry the module's blessed marker on their own line: the
//! shard-directory `create_dir_all` in [`Cas::put`], the `create_new` open, and the
//! `write_all` of the bytes. That guard enumerates them individually — see its module
//! doc for why one write-only store needs more than one blessed needle.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The URI prefix an object is addressed by: `kaibo://cas/<digest>`. Declared here,
/// beside the store, so every producer that *renders* an address (`generate`'s result
/// lines, `save_artifact`'s footer) spells it one way.
///
/// A **name, not a route.** It was an MCP resource until 2026-08-05; retrieval is now the
/// `read_cas` tool, which takes the digest out of this string. Reading is **operator
/// surface only** (Amy's ruling, 2026-08-03), and that survived the move: the MCP client
/// retrieves; the inner model team never can. There is no CLI artifact command — on disk,
/// an operator reaches an object by the path a read reports. The CAS is not mounted
/// into kaish and no cast-facing tool reads it, because kaibo state spans projects and a
/// browsable CAS would let one project's team enumerate another's artifacts.
pub const CAS_URI_PREFIX: &str = "kaibo://cas/";

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
    /// The requested CAS root — or a path component on the way to it — exists but is
    /// not a directory, so the store structurally cannot live there. Caught at
    /// [`Cas::open`] so it fails startup, not the first paid generation.
    #[error(
        "media CAS path cannot be used: {0} is not a directory — the store needs a \
         directory (or a creatable path whose existing ancestors are directories)"
    )]
    NotADirectory(String),
    /// A filesystem operation failed for a reason other than the ones above (permissions,
    /// disk full, an unreadable existing file, …), surfaced verbatim.
    #[error("media CAS io: {0}")]
    Io(String),
    /// An object exists at the path its digest addresses, but the bytes read back from that
    /// path do not hash to that digest. This is the corruption case the module doc's write
    /// path cannot fully rule out on its own (see [`write_new_file`]): a crash or power loss
    /// between `create_new` and a completed `write_all` can leave a truncated or partial file
    /// sitting at a content-addressed path, and nothing about the write side can detect that
    /// after the fact — the path existing was, until this variant existed, silently treated
    /// as proof the bytes were good (`AlreadyExists` mapped straight to `Ok`). There is no
    /// recovery *in place*: this store never unlinks, truncates, or renames (see the module
    /// doc), so a poisoned object stays poisoned at this address forever — the operator must
    /// intervene by hand (remove the bad file and re-`put` the content) once this fires.
    #[error(
        "media CAS object corrupt: expected digest {expected}, but the bytes at this path hash \
         to {actual} — the object is unrecoverable in place (this store never rewrites; remove \
         the file by hand and re-put the content)"
    )]
    Corrupt { expected: String, actual: String },
    /// The provenance sidecar failed to serialize to JSON. `Provenance`'s fields are
    /// plain strings/ints, so this should not happen in practice; surfaced loudly rather
    /// than silently dropping the sidecar (which would defeat the whole GC-by-sidecar
    /// story — see the module doc).
    #[error("media CAS provenance sidecar serialize: {0}")]
    Serialize(String),
    /// **The object landed; its provenance sidecar did not.** [`Cas::put`] writes the
    /// object first and the sidecar second, so an I/O failure between them leaves the
    /// content durably stored and retrievable — the probe fallback in
    /// [`Cas::entry_for`] finds a sidecar-less object — while the call still has to
    /// report a failure, because the provenance record the store's whole no-GC stance
    /// rests on is missing.
    ///
    /// Its own variant, rather than a plain [`CasError::Io`], because the two demand
    /// opposite things of a caller: an `Io` failure means the bytes are not there and
    /// "nothing was saved" is true, while this means they ARE there and saying nothing
    /// was saved is a lie about durable data. The digest rides along because it is the
    /// only way back to bytes that no longer describe themselves.
    #[error(
        "media CAS object {digest} was stored, but its provenance sidecar could not be \
         written ({cause}) — the bytes are durable and retrievable at that digest, and \
         this store never rewrites, so the record cannot be filled in later"
    )]
    ProvenanceNotRecorded { digest: String, cause: String },
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

/// An artifact's on-disk container format. Deliberately a closed enum, not a
/// caller-supplied string: this is the *only* caller-influenced component of an
/// object's path (the digest supplies the rest), so it must be a small, fixed set kaibo
/// controls — a free string here would reopen the aim-the-write hole the digest closes
/// for the rest of the path. Extend this list as new producers/formats land; never
/// widen it to `String`.
///
/// The image variants come from `generate` (a provider rendered them); the text
/// variants come from `save_artifact` (a model on kaibo's own team authored them).
/// [`Extension::is_image`] tells them apart, which is what keeps `generate`'s "an
/// images provider returned something that is not an image" refusal exactly as loud as
/// it was when this enum held images alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extension {
    Png,
    Jpeg,
    Webp,
    Gif,
    /// Plain UTF-8 text: a model-authored report, corpus, or listing.
    Txt,
    /// JSON Lines: one JSON value per line, the shape a machine consumer streams.
    Jsonl,
    /// Markdown: the shape a model naturally writes a report in.
    Md,
}

impl Extension {
    /// Every variant, in the order [`Cas::entry_for`] probes them when an object has no
    /// readable provenance sidecar to name its format. The sidecar is the authority;
    /// this order only decides the answer for an object that lost one.
    pub const ALL: [Extension; 7] = [
        Extension::Png,
        Extension::Jpeg,
        Extension::Webp,
        Extension::Gif,
        Extension::Txt,
        Extension::Jsonl,
        Extension::Md,
    ];

    /// The bare filename extension (no leading dot).
    pub fn as_str(&self) -> &'static str {
        match self {
            Extension::Png => "png",
            Extension::Jpeg => "jpeg",
            Extension::Webp => "webp",
            Extension::Gif => "gif",
            Extension::Txt => "txt",
            Extension::Jsonl => "jsonl",
            Extension::Md => "md",
        }
    }

    /// The canonical mime type this on-disk format serves as — what the
    /// `read_cas` reports for an object it hands back, and what a
    /// producer records in the provenance sidecar so [`Cas::entry_for`] can read the
    /// format straight back out.
    pub fn mime(&self) -> &'static str {
        match self {
            Extension::Png => "image/png",
            Extension::Jpeg => "image/jpeg",
            Extension::Webp => "image/webp",
            Extension::Gif => "image/gif",
            Extension::Txt => "text/plain; charset=utf-8",
            Extension::Jsonl => "application/jsonl",
            Extension::Md => "text/markdown; charset=utf-8",
        }
    }

    /// Is this a rendered image (a `generate` output) rather than authored text?
    /// `generate` prevalidates on this, so an images provider handing back a
    /// `text/plain` body is still refused loudly instead of stored as an artifact:
    /// growing this enum for `save_artifact` must not quietly widen what the media
    /// lane accepts.
    pub fn is_image(&self) -> bool {
        match self {
            Extension::Png | Extension::Jpeg | Extension::Webp | Extension::Gif => true,
            Extension::Txt | Extension::Jsonl | Extension::Md => false,
        }
    }

    /// Should retrieval serve this format as a **string** rather than base64? The
    /// producers mark what they make — `save_artifact` writes these formats from UTF-8
    /// input, `generate` writes images — and this is where retrieval reads the mark.
    /// Deliberately not `!is_image()`: a future format (audio, an archive) is neither
    /// an image nor text, so each new variant must answer both questions on its own.
    pub fn is_textual(&self) -> bool {
        match self {
            Extension::Txt | Extension::Jsonl | Extension::Md => true,
            Extension::Png | Extension::Jpeg | Extension::Webp | Extension::Gif => false,
        }
    }

    /// Map a wire mime string (a producer's own spelling, case-insensitive per RFC
    /// 7231) onto the closed on-disk set. `None` for anything the CAS cannot name on
    /// disk — the caller decides loudly what that means (see
    /// `stability::MediaType::to_cas_extension` for the same refusal argued at length);
    /// this helper never invents an extension for unknown bytes.
    ///
    /// Parameters are dropped before matching, so our own canonical
    /// `text/plain; charset=utf-8` and a bare `text/plain` name the same format. The
    /// JSON Lines aliases are all accepted on parse because that format never got one
    /// registered spelling; [`Extension::mime`] renders exactly one of them.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let essence = mime.split(';').next().unwrap_or(mime).trim();
        match essence.to_ascii_lowercase().as_str() {
            "image/png" => Some(Extension::Png),
            "image/jpeg" => Some(Extension::Jpeg),
            "image/webp" => Some(Extension::Webp),
            "image/gif" => Some(Extension::Gif),
            "text/plain" => Some(Extension::Txt),
            "text/markdown" | "text/x-markdown" => Some(Extension::Md),
            "application/jsonl"
            | "application/x-ndjson"
            | "application/jsonlines"
            | "application/x-jsonlines" => Some(Extension::Jsonl),
            _ => None,
        }
    }
}

/// The provenance sidecar recorded next to every object: `prompt`, `model`, `cast`,
/// `timestamp` (epoch seconds), `mime`, `seed`, and the authorship fields that say which
/// tool produced it and, for authored text, who wrote it. This is the whole point of shipping
/// with no built-in GC: an object reached **by its address** describes itself, instead of
/// being opaque hashed bytes. Read it by digest, the way everything in this store is read
/// — not by sweeping the tree, which does not scale and is not what this is for. See the
/// module doc's "No GC here, on purpose" section.
///
/// `mime` is also load-bearing at runtime: it is what [`Cas::entry_for`] reads to name
/// an object's format in one lookup rather than probing every container format kaibo
/// knows. Record the format the object actually is (`Extension::mime` is the canonical
/// spelling) — a sidecar that disagrees with the bytes beside it mislabels the artifact
/// on retrieval.
///
/// # Every new field must be `Option`, or carry `#[serde(default)]`
///
/// The CAS never deletes, so a sidecar written by *today's* kaibo must stay readable by
/// *every future* kaibo — there is no migration pass available to us, because rewriting a
/// sidecar is a write we have structurally forbidden. A new **required** field would make
/// every previously-written sidecar fail to deserialize, silently turning a user's
/// paid-for archive into unreadable bytes. That is the exact failure the stewardship
/// stance exists to prevent, so this is a hard rule, not a convenience.
///
/// Serde already treats an `Option` field as optional, so `Option` alone satisfies the
/// rule and an added `#[serde(default)]` on one is redundant — worth stating, because the
/// redundant spelling reads like the thing doing the work and invites a future field to
/// rely on the attribute while being non-`Option` in a way that *does* break old
/// sidecars. The rule is about the field's optionality, not the attribute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub prompt: String,
    pub model: String,
    pub cast: String,
    pub timestamp: i64,
    pub mime: String,
    /// The provider-reported seed, when it reports one — the single most valuable field
    /// here. The stewardship argument for the whole CAS is that a generated artifact may
    /// be *irreproducible*; the seed is precisely what makes one reproducible, so
    /// dropping it while carefully preserving the bytes would be perverse. `Option`
    /// because not every provider or operation reports one (an upscale has no seed of
    /// its own), and `String` rather than an integer because it is an opaque provider
    /// token we echo back, never arithmetic we perform.
    pub seed: Option<String>,

    // --- Authorship: who made these bytes, and by which route --------------
    //
    // Downstream, someone will *execute* an artifact's contents — the motivating
    // case for `save_artifact` is a corpus of shell commands. The sidecar is the
    // record they decide trust from, so it has to distinguish a provider's render
    // from a model's writing. `model` and `cast` already name the team; these name
    // the route and the intent.
    //
    // Each is written only when it applies, so a sidecar carries no null-filled
    // fields it has nothing to say about, and a `generate` sidecar's shape is what
    // it always was apart from the `tool` line that now names its producer.
    /// The kaibo tool that recorded this artifact: `generate` for a provider render,
    /// `save_artifact` for bytes a model on kaibo's own team wrote. Absent on any
    /// sidecar written before this field existed, which means `generate` — the only
    /// producer kaibo had then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Which reasoning slot the authoring model filled (`synth` today — only the
    /// consult driver loop can save). Absent for a provider render, which fills the
    /// `image` slot and has no author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// The authoring model's own one-line description of what it saved. Written by
    /// the model, so read it as a claim rather than a fact — it is what makes a
    /// hand-pruning pass over the store legible, next to a `prompt` that describes
    /// the question rather than this artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The consult session this artifact was authored in, when the call carried one.
    /// Ties an artifact back to the conversation that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A content-addressed store of generated media artifacts, rooted at a fixed,
/// model-inaccessible directory. See the module doc for the full safety argument;
/// briefly: the address is the SHA-256 of the content, so no `pub` method here takes a
/// destination path, and every write is `create_new` (never a truncate/overwrite).
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
    /// The optional soft cap. `None` — the default — means no ceiling AND no size
    /// accounting: [`Cas::put`] never walks the store. See [`Cas::open`].
    max_bytes: Option<u64>,
    /// Serializes [`Cas::put`] across threads sharing this store (clones share it —
    /// the handler holds one `Arc<MediaStore>` across concurrent MCP calls).
    /// Generation is rare and writes are small, so a plain mutex buys the whole
    /// check-then-write path (dedup verify, cap accounting, object + sidecar) its
    /// atomicity with no cleverness. See [`Cas::put`].
    write_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl Cas {
    /// Open (but do not yet create — see below) the CAS rooted at `dir`, refusing if
    /// `dir` resolves inside any `allowed_trees` entry (the read-only sandbox's project
    /// roots).
    ///
    /// `max_bytes` is an **optional** soft cap, and `None` (the default) is meaningfully
    /// different from a very large number: it means [`Cas::put`] does no size accounting
    /// at all. That matters because enforcing a cap requires summing every file in the
    /// store, on the *write* path, for every new object — over a store that by design
    /// never deletes anything, sharded two levels deep. That walk only gets slower as the
    /// store fills, which is exactly the cost an operator who never asked for a ceiling
    /// should not pay. Amy's call (2026-07-30): leave the accounting off until someone has
    /// a problem that needs it. Disk-full is the real backstop and the OS reports it
    /// honestly, and kaibo ships no pruning verb (see the module doc's "No GC here, on
    /// purpose").
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
    pub fn open(dir: &Path, allowed_trees: &[&Path], max_bytes: Option<u64>) -> Result<Self> {
        let dir = absolutize(dir)?;
        let resolved = resolve_existing_ancestor(&dir);
        for tree in allowed_trees {
            let tree_resolved = tree.canonicalize().unwrap_or_else(|_| normalize(tree));
            if resolved.starts_with(&tree_resolved) {
                return Err(CasError::PathInAllowedTree(dir.display().to_string()));
            }
        }
        // Structural check, still with zero side effects: the deepest EXISTING path on
        // the way to the root must be a directory (the root itself included, when it
        // exists). A file sitting there means every future write must fail — caught
        // here so it fails startup, not the first paid generation. What this can NOT
        // vouch for without writing is writability (permissions, disk full, a
        // read-only mount); those still surface on first use.
        if let Some(existing) = first_existing_ancestor(&dir) {
            if !existing.is_dir() {
                return Err(CasError::NotADirectory(existing.display().to_string()));
            }
        }
        Ok(Self {
            root: dir,
            max_bytes,
            write_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
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

    /// The path an object would live at, if it exists. `None` if no object with this
    /// digest has ever been written (or an ancestor directory doesn't exist yet, which
    /// reads the same way: nothing is here).
    pub fn path_for(&self, digest: &Digest) -> Option<PathBuf> {
        self.entry_for(digest).map(|(path, _)| path)
    }

    /// Read an object's provenance sidecar, if one is there and parses. `None` covers
    /// three cases the caller treats alike — no sidecar (an object whose write crashed
    /// between the two files; see [`write_new_file`]), an unreadable one, and one whose
    /// JSON doesn't deserialize — because each means the same thing to a lookup: this
    /// object cannot tell us what it is, so something else has to.
    pub fn provenance_for(&self, digest: &Digest) -> Option<Provenance> {
        let bytes = std::fs::read(self.sidecar_path(digest)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// [`path_for`](Self::path_for) plus which [`Extension`] the object was written
    /// under — the extension carries the mime type a read needs to report for the
    /// bytes, and re-deriving it from the path string would be a second parser.
    ///
    /// **The sidecar is the authority.** Every object written by this store gets a
    /// `<hex>.json` sidecar whose `mime` describes it, so one read of that file names
    /// the format directly: one lookup, not a walk of every container format kaibo
    /// knows. That matters more as the set grows past images — a probe answers with
    /// whichever variant it happens to try first, which for content stored under two
    /// names is a *mislabel* `read_cas` then reports for the
    /// bytes, and for a large set is N stats per read.
    ///
    /// **The probe is the fallback, never the requirement.** An object whose sidecar
    /// is missing, unreadable, or names a mime this kaibo cannot map still resolves by
    /// trying [`Extension::ALL`]. This store never rewrites, so an old or orphaned
    /// object can't be migrated into the new scheme — losing one to a metadata gap
    /// would be exactly the silent data loss the module refuses.
    pub fn entry_for(&self, digest: &Digest) -> Option<(PathBuf, Extension)> {
        if let Some(ext) = self
            .provenance_for(digest)
            .and_then(|p| Extension::from_mime(&p.mime))
        {
            let path = self.object_path(digest, ext);
            if path.is_file() {
                return Some((path, ext));
            }
        }
        Extension::ALL.into_iter().find_map(|ext| {
            let path = self.object_path(digest, ext);
            path.is_file().then_some((path, ext))
        })
    }

    /// Read an object's bytes back by digest, **verifying** them against that digest before
    /// returning anything: `Ok(Some(bytes))` means an object exists at this digest's path
    /// *and* its content hashes to it; `Ok(None)` means nothing with this digest has ever
    /// been written; `Err(CasError::Corrupt)` means a file exists at the address but its
    /// bytes do not hash to it (see [`write_new_file`]'s fsync-window note — a crash mid-write
    /// is exactly how this happens); `Err(CasError::Io)` means the object could not even be
    /// read (permissions, disk error). None of these fold into each other: an I/O error is
    /// not "missing" and a hash mismatch is not "missing" either — both are surfaced loudly
    /// rather than silently treated as "nothing here," which is the silent-fallback shape
    /// this store now refuses to repeat (see [`CasError::Corrupt`]'s doc and the module doc's
    /// "why verification is possible here" note).
    ///
    /// Verification is affordable *because this call slurps the whole object into memory
    /// before returning it* — the hash can be checked against every byte before the first one
    /// reaches the caller. A future streaming read (open a handle, hand back a reader) could
    /// not inherit this guarantee for free: by the time a streaming reader discovered a
    /// mismatch at the end of the stream, it would already have emitted unverified bytes to
    /// whoever was consuming it. Adding a streaming read path means solving *that* problem
    /// (failing the stream on mismatch, and requiring every caller to handle a mid-stream
    /// integrity failure) rather than carrying this function's contract over unexamined.
    pub fn get(&self, digest: &Digest) -> Result<Option<Vec<u8>>> {
        let Some(path) = self.path_for(digest) else {
            return Ok(None);
        };
        let bytes = std::fs::read(&path)
            .map_err(|e| CasError::Io(format!("reading cas object {}: {e}", path.display())))?;
        verify(digest, bytes).map(Some)
    }

    /// Recursively sum the size in bytes of every file currently in the store. Backs the
    /// soft-cap check in [`Cas::put`], and is called ONLY when a cap is configured — see
    /// [`Cas::open`] for why an uncapped store must never pay for this. A read-only
    /// directory walk: no writes, no caching, no separate counter file to keep in sync
    /// (and thus no extra write site this module would otherwise need). The flip side of
    /// holding no counter is that the cost is O(objects) per capped write, which is
    /// precisely why the cap is opt-in rather than a default ceiling.
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
    /// object with this exact digest already exists, this is a **verified** no-op: the
    /// existing bytes are read back and checked against `digest` before anything is
    /// reported as success — same hash means same bytes only if the object on disk is
    /// actually intact, which a bare path-exists check cannot tell (see
    /// [`CasError::Corrupt`]). If that check fails, the write returns
    /// `Err(CasError::Corrupt)` rather than silently discarding the caller's good bytes
    /// into a poisoned slot — the caller's data was never at risk, only the report of
    /// success was.
    ///
    /// # The cap admits every put the same way, dedup or not
    ///
    /// When a cap is configured it is checked *before* any write, counting the whole
    /// footprint this put would add — the artifact **and** its provenance sidecar, which
    /// is real disk usage written by the same call — and a put past it is refused with
    /// [`CasError::CapacityExceeded`], nothing written and nothing evicted.
    ///
    /// That check runs **whether or not the content is already here**, which is a
    /// deliberate reversal (2026-08-05). Exempting a dedup is correct about disk usage —
    /// re-putting held content adds no bytes — and wrong about disclosure. Once a *model*
    /// can drive puts (`save_artifact`), "succeeded" versus "refused" at a full store
    /// answers *is this content already in the store?* for arbitrary bytes, over a store
    /// that spans every project this kaibo has served. Making admission independent of
    /// what the store holds closes that oracle: a full store is uniformly closed. The
    /// price is a wasted no-op write at exactly-full, which loses nothing.
    ///
    /// The whole body runs under the store's write mutex, so concurrent puts on
    /// clones/`Arc`s of one store serialize: the dedup check, the cap accounting, and
    /// the two writes are atomic with respect to each other. If `create_new` still
    /// loses to something that appeared from outside this process (another kaibo, an
    /// operator's copy), the existing bytes are read back and **verified** before
    /// success is reported — never trusted from the path alone.
    ///
    /// # One failure that is not a failure to store
    ///
    /// The object is written before the sidecar, so a sidecar write that fails leaves the
    /// content durable and retrievable. That returns
    /// [`CasError::ProvenanceNotRecorded`] — carrying the digest — rather than a plain
    /// [`CasError::Io`], because a caller that renders "nothing was saved" from it would
    /// be lying about data that is on disk and reachable.
    pub fn put(&self, bytes: &[u8], ext: Extension, provenance: &Provenance) -> Result<Digest> {
        let digest = Digest::of_bytes(bytes);
        let shard = self.shard_dir(&digest);
        let obj_path = self.object_path(&digest, ext);
        let sidecar_path = self.sidecar_path(&digest);

        let _write_guard = self.write_lock.lock().expect("cas write mutex poisoned");

        // Serialized up front so the cap admission below can count it — the sidecar is
        // part of this put's disk footprint, not an afterthought.
        let sidecar_json = serde_json::to_vec_pretty(provenance)
            .map_err(|e| CasError::Serialize(e.to_string()))?;

        // Cap admission FIRST, and unconditionally when a cap is configured — before the
        // dedup read below, so nothing about the outcome depends on whether these bytes
        // are already here. See the "admits every put the same way" section above: a
        // dedup exemption is an existence oracle over every project's artifacts. This is
        // still the ONLY call site that walks the store, and it is still reached only
        // when an operator opted into a ceiling; an uncapped CAS never sizes itself.
        if let Some(max_bytes) = self.max_bytes {
            let current = self.total_bytes()?;
            let incoming = bytes.len() as u64 + sidecar_json.len() as u64;
            if current.saturating_add(incoming) > max_bytes {
                return Err(CasError::CapacityExceeded {
                    max_bytes,
                    current_bytes: current,
                });
            }
        }

        if obj_path.is_file() {
            // Dedup path: the module doc's "AlreadyExists means these exact bytes are
            // already here" claim only holds if every object at this path was placed by
            // this CAS *and placed completely* — neither half is actually enforced (see
            // CasError::Corrupt's doc), so verify rather than trust the path's existence
            // before reporting success. A caller whose good bytes silently vanish into a
            // poisoned slot is the worst failure mode this module can produce; refuse to
            // produce it. Verified, we still fall through to the (no-op) write below — it
            // costs nothing on a healthy object, and covers the edge case of a sidecar that
            // is somehow missing next to an otherwise-good object.
            let existing = std::fs::read(&obj_path).map_err(|e| {
                CasError::Io(format!(
                    "reading existing cas object {} to verify dedup: {e}",
                    obj_path.display()
                ))
            })?;
            verify(&digest, existing)?;
        }

        std::fs::create_dir_all(&shard) // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
            .map_err(|e| {
                CasError::Io(format!("creating cas shard dir {}: {e}", shard.display()))
            })?;

        let wrote = write_new_file(&obj_path, bytes)
            .map_err(|e| CasError::Io(format!("writing cas object {}: {e}", obj_path.display())))?;
        if !wrote {
            // The pre-check saw nothing here, yet `create_new` lost: something claimed
            // the path from outside this process (the in-process race is excluded by
            // the mutex above). The contract stands — an existing object is a VERIFIED
            // no-op — so read back and verify before reporting success. A path that
            // exists but cannot even be read (a dangling symlink, permissions) is a
            // loud Io error, never a silent "someone else surely wrote it".
            let existing = std::fs::read(&obj_path).map_err(|e| {
                CasError::Io(format!(
                    "cas object {} was claimed concurrently but cannot be read back to \
                     verify: {e}",
                    obj_path.display()
                ))
            })?;
            verify(&digest, existing)?;
        }

        // Past this point the object IS stored. A sidecar failure is still a failure —
        // the provenance record is what makes the no-GC stance honest — but it is not a
        // failure to store, and reporting it as a bare Io error invites a caller to tell
        // its user "nothing was saved" about durable, reachable bytes. Carry the digest.
        write_new_file(&sidecar_path, &sidecar_json).map_err(|e| {
            CasError::ProvenanceNotRecorded {
                digest: digest.to_hex(),
                cause: e.to_string(),
            }
        })?;

        Ok(digest)
    }
}

/// One artifact held by the in-memory store: the bytes plus the metadata a disk
/// object keeps in its filename (`ext`) and sidecar (`provenance`).
#[derive(Debug, Clone)]
struct MemObject {
    bytes: Vec<u8>,
    ext: Extension,
    provenance: Provenance,
    /// Size of `provenance` in its serialized form — the same representation the disk
    /// store writes to a sidecar. Held rather than recomputed so the cap sum stays a
    /// cheap add, and held at all because memory mode really does keep this data: a cap
    /// that counted only content bytes would let a capped store hold more than the
    /// operator asked for, and would make one `max_bytes` mean two different things in
    /// the two modes.
    provenance_bytes: u64,
}

/// The in-memory CAS: the same content-addressed contract as [`Cas`] — the address is
/// the content's own hash, `put` takes no destination, nothing is ever deleted or
/// rewritten — held in a `HashMap` instead of on disk. This is the degraded mode Amy
/// decided for a run without persistence ("cas should be on when persistence is, and
/// ideally on but in memory only when it's not", 2026-07-30): artifacts stay
/// retrievable by digest for the life of the process and are gone on restart, which
/// startup warns about LOUDLY (see `main.rs`) and `kaibo://config` reports as
/// `mode = "memory"`. Touches no filesystem at all, so the write-path guard
/// (`tests/no_write_path.rs`) has nothing to bless here.
///
/// The optional `max_bytes` cap is honored with the same refuse-never-evict posture as
/// [`Cas::put`] — in memory the accounting is a cheap sum over held objects, but the
/// *meaning* of the knob (a ceiling that refuses, loudly) must not change with the mode.
#[derive(Debug)]
pub struct MemoryCas {
    objects: std::sync::Mutex<std::collections::HashMap<Digest, MemObject>>,
    max_bytes: Option<u64>,
}

impl MemoryCas {
    pub fn new(max_bytes: Option<u64>) -> Self {
        Self {
            objects: std::sync::Mutex::new(std::collections::HashMap::new()),
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<Digest, MemObject>> {
        self.objects.lock().expect("memory CAS mutex poisoned")
    }

    /// Store `bytes` under their own digest. A repeat of identical content is a
    /// no-op returning the same digest (dedup needs no verification here: the map is
    /// keyed by the digest of exactly the bytes it holds, and nothing between put and
    /// get can corrupt process memory the way a torn disk write can).
    ///
    /// The cap is checked **before** the dedup short-circuit and counts the provenance
    /// alongside the content, both mirroring [`Cas::put`] exactly. The admission order is
    /// the load-bearing half: short-circuiting on "already held" before the cap would let
    /// a full store answer *is this content here?* by succeeding for held bytes and
    /// refusing for new ones — an existence oracle over every project's artifacts. One
    /// meaning per knob, one outcome per full store, in both modes.
    pub fn put(&self, bytes: &[u8], ext: Extension, provenance: &Provenance) -> Result<Digest> {
        let digest = Digest::of_bytes(bytes);
        // Serialized the same way the disk sidecar is, so `max_bytes` admits the same
        // footprint in either mode.
        let provenance_bytes = serde_json::to_vec_pretty(provenance)
            .map_err(|e| CasError::Serialize(e.to_string()))?
            .len() as u64;
        let mut objects = self.lock();
        if let Some(max_bytes) = self.max_bytes {
            let current: u64 = objects
                .values()
                .map(|o| o.bytes.len() as u64 + o.provenance_bytes)
                .sum();
            let incoming = bytes.len() as u64 + provenance_bytes;
            if current.saturating_add(incoming) > max_bytes {
                return Err(CasError::CapacityExceeded {
                    max_bytes,
                    current_bytes: current,
                });
            }
        }
        if objects.contains_key(&digest) {
            return Ok(digest);
        }
        objects.insert(
            digest,
            MemObject {
                bytes: bytes.to_vec(),
                ext,
                provenance: provenance.clone(),
                provenance_bytes,
            },
        );
        Ok(digest)
    }

    /// Read an object back by digest: the bytes and the extension it was stored
    /// under. `None` if nothing with this digest was ever put.
    pub fn get(&self, digest: &Digest) -> Option<(Vec<u8>, Extension)> {
        self.lock().get(digest).map(|o| (o.bytes.clone(), o.ext))
    }

    /// The provenance recorded with an object, if it exists — the in-memory analogue
    /// of reading a disk sidecar.
    pub fn provenance(&self, digest: &Digest) -> Option<Provenance> {
        self.lock().get(digest).map(|o| o.provenance.clone())
    }

    /// The container format this digest is held under, without copying its bytes — the
    /// in-memory analogue of [`Cas::entry_for`]'s extension half.
    pub fn extension_for(&self, digest: &Digest) -> Option<Extension> {
        self.lock().get(digest).map(|o| o.ext)
    }
}

/// The media store a running kaibo actually holds: the disk [`Cas`] when persistence
/// is active, the [`MemoryCas`] when it is not. One seam so every consumer (the
/// generate lane, the `read_cas` tool, the `kaibo://config` render)
/// dispatches over the mode instead of each carrying its own two-armed match — and so
/// the *contract* (content-addressed, write-only, no destination parameter) is the
/// same sentence in both modes.
#[derive(Debug)]
pub enum MediaStore {
    Disk(Cas),
    Memory(MemoryCas),
}

impl MediaStore {
    /// Store an artifact; the digest is its address in either mode.
    pub fn put(&self, bytes: &[u8], ext: Extension, provenance: &Provenance) -> Result<Digest> {
        match self {
            MediaStore::Disk(cas) => cas.put(bytes, ext, provenance),
            MediaStore::Memory(mem) => mem.put(bytes, ext, provenance),
        }
    }

    /// **The store's own answer for what an object IS**, without reading its bytes: the
    /// container format, and through it the mime `read_cas` will
    /// stamp and the extension the on-disk path carries.
    ///
    /// A producer's *requested* format is not that answer. The address is the content
    /// hash, so identical bytes saved as `jsonl` after they were already stored as `txt`
    /// land at the same digest; the second put writes a second container file while the
    /// sidecar — the authority, see [`Cas::entry_for`] — still says `txt`. A caller that
    /// rendered its own request would advertise `application/jsonl` beside a `.txt` path
    /// and a `text/plain` read. Ask the store instead, always.
    ///
    /// Refusing the second put would be the other way to fix that, and it is worse: the
    /// refusal itself would reveal that the content was already present.
    pub fn extension_for(&self, digest: &Digest) -> Option<Extension> {
        match self {
            MediaStore::Disk(cas) => cas.entry_for(digest).map(|(_, ext)| ext),
            MediaStore::Memory(mem) => mem.extension_for(digest),
        }
    }

    /// The housekeeping record beside an object, in whichever mode holds it — the disk
    /// sidecar, or the memory store's clone of it. `None` when the object is unknown, or
    /// (on disk) when its sidecar is missing or unreadable, which
    /// [`Cas::provenance_for`] treats alike because a lookup can do nothing with any of
    /// the three.
    ///
    /// Read by address, like everything else here: this is what makes an object
    /// self-describing to whoever holds its digest — `read_cas` takes the label from it.
    pub fn provenance(&self, digest: &Digest) -> Option<Provenance> {
        match self {
            MediaStore::Disk(cas) => cas.provenance_for(digest),
            MediaStore::Memory(mem) => mem.provenance(digest),
        }
    }

    /// Read an object back with the extension (and thus mime) it was stored under.
    /// Disk reads verify content against the digest ([`Cas::get`]'s contract); memory
    /// reads need no verification (see [`MemoryCas::put`]).
    pub fn get(&self, digest: &Digest) -> Result<Option<(Vec<u8>, Extension)>> {
        match self {
            MediaStore::Disk(cas) => {
                let Some((_, ext)) = cas.entry_for(digest) else {
                    return Ok(None);
                };
                Ok(cas.get(digest)?.map(|bytes| (bytes, ext)))
            }
            MediaStore::Memory(mem) => Ok(mem.get(digest)),
        }
    }

    /// The real filesystem path of an object — `Some` only in disk mode, where an
    /// operator (or the calling agent, acting as the operator's proxy) can reach the
    /// file directly. Memory mode has no path; `read_cas` is the only retrieval channel
    /// there.
    pub fn path_for(&self, digest: &Digest) -> Option<PathBuf> {
        match self {
            MediaStore::Disk(cas) => cas.path_for(digest),
            MediaStore::Memory(_) => None,
        }
    }

    /// The store's root directory — `Some` only in disk mode.
    pub fn root(&self) -> Option<&Path> {
        match self {
            MediaStore::Disk(cas) => Some(cas.root()),
            MediaStore::Memory(_) => None,
        }
    }

    /// The mode word `kaibo://config` renders: `"disk"` or `"memory"`.
    pub fn mode(&self) -> &'static str {
        match self {
            MediaStore::Disk(_) => "disk",
            MediaStore::Memory(_) => "memory",
        }
    }
}

/// Create `path` as a brand-new file (`create_new` — `O_EXCL` on Unix), write `bytes` to
/// it, and `sync_all` before returning — pushing both data and metadata to durable storage
/// rather than leaving them in a page-cache buffer a crash could still lose. Returns
/// `Ok(true)` when this call wrote the file, `Ok(false)` when `path` already existed
/// (nothing written) — the CALLER decides what an existing path means: [`Cas::put`]
/// verifies an existing object's bytes before reporting success (see
/// [`CasError::Corrupt`]), and treats an existing sidecar as fine. Any other I/O
/// failure (permissions, disk full, the parent directory missing) is propagated.
/// There is no unlink, truncate, or rename anywhere in this
/// function or its caller — an "edit" is always a new digest, never a mutation of an
/// existing object.
///
/// `sync_all` **shrinks** the crash window between `open` and durable bytes on disk; it
/// does not close it. A crash can still land between `write_all` succeeding and `sync_all`
/// completing, or (rarer, but real on some filesystems/hardware) `sync_all` can itself
/// return `Ok` ahead of what the physical medium has actually made durable. That residual
/// window is exactly why verification on read ([`Cas::get`]) and on dedup ([`Cas::put`])
/// exists as the actual backstop — fsync narrows how often corruption occurs, verification
/// is what guarantees it is never handed back silently when it does.
fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    use std::io::Write;

    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(path) // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    file.write_all(bytes)?; // media-cas-write: blessed by the media CAS invariant amendment (AGENTS.md)
    file.sync_all()?;
    Ok(true)
}

/// Verify that `bytes` hashes to `digest`, consuming `bytes` into the `Ok` case so a
/// caller doesn't have to clone just to satisfy the borrow checker across the check.
/// Shared by [`Cas::get`] (verify-before-return) and [`Cas::put`] (verify-before-dedup) —
/// one place computing the "does this content match its address" predicate, since both
/// call sites need the exact same answer for the exact same reason.
fn verify(digest: &Digest, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let actual = Digest::of_bytes(&bytes);
    if actual != *digest {
        return Err(CasError::Corrupt {
            expected: digest.to_hex(),
            actual: actual.to_hex(),
        });
    }
    Ok(bytes)
}

// --- The backing-filesystem guard -------------------------------------------
//
// Disk mode means "artifacts survive a restart", and on a container with no volume
// mounted that sentence is false while every other signal says it is true: persistence
// comes up, `Cas::open` succeeds, writes land, reads verify. overlayfs behaves exactly
// like ext4 right up to the moment the container exits and takes the store with it. So
// the store cannot notice from the inside — the only tell is what the directory is
// sitting on, and that is a question for startup, once, out of band.
//
// Amy's design ruling (2026-07-30): **warn severely, then proceed.** Not a refusal, and
// not gated on an acknowledgement flag. Running on tmpfs is a legitimate thing to do
// deliberately (a scratch run, a test rig, a cache you mean to throw away); what is not
// legitimate is *discovering* it after paying for a generation. The guard's whole job is
// to make sure the operator heard it.

/// What the filesystem under an object store turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// A filesystem whose contents die with the process's container or host: overlayfs
    /// with no volume, tmpfs, ramfs. Carries the name so the warning can say which.
    Ephemeral { fs: &'static str },
    /// Nothing here says the store is ephemeral.
    Durable,
    /// The probe could not answer — an unsupported platform, a stat failure, a path with
    /// an interior NUL.
    ///
    /// **Deliberately not folded into either other arm.** Treating it as ephemeral would
    /// warn on every non-Linux install and train operators to ignore the one message
    /// that has to land; treating it as durable would *claim* something the probe never
    /// established. Not knowing is its own answer, and the only correct response to it
    /// is silence (at debug, for whoever is actually debugging).
    Unknown,
}

impl Backing {
    /// The ephemeral filesystem's name, or `None` for anything that is not a positive
    /// ephemerality finding. The predicate every caller wants: only `Ephemeral` speaks.
    pub fn ephemeral_fs(&self) -> Option<&'static str> {
        match self {
            Backing::Ephemeral { fs } => Some(fs),
            Backing::Durable | Backing::Unknown => None,
        }
    }
}

/// The startup warning for a CAS sitting on an ephemeral filesystem.
///
/// Pure, so its content is testable without a subscriber, and separate from the
/// `tracing` call so the *wording* is a thing with a test rather than a string literal
/// buried in a match arm. It carries four facts because an operator gets one chance to
/// read this: which filesystem, which directory, what happens to the artifacts, and the
/// fix.
pub fn ephemeral_backing_warning(fs: &str, dir: &Path) -> String {
    format!(
        "MEDIA CAS IS ON AN EPHEMERAL FILESYSTEM: {dir} is backed by {fs}. kaibo will \
         store generated artifacts there and they will read back correctly for this run, \
         but they will NOT survive this container or host — they are paid for with real \
         provider credits and may not be reproducible. If you meant this (a scratch run), \
         carry on; if you did not, mount a volume at that path (or point [cas] dir at one) \
         and restart. kaibo://config reports this under [cas] backing.",
        dir = dir.display(),
    )
}

/// Ask what filesystem `dir` is sitting on.
///
/// Answers for a directory that **does not exist yet** by asking about its nearest
/// existing ancestor: the CAS root is created lazily on the first write, so a probe that
/// insisted on the leaf would return [`Backing::Unknown`] on every fresh install — which
/// is precisely the fresh-container case this guard exists for.
#[cfg(target_os = "linux")]
pub fn probe_backing(dir: &Path) -> Backing {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Some(existing) = first_existing_ancestor(dir) else {
        return Backing::Unknown;
    };
    let Ok(c_path) = CString::new(existing.as_os_str().as_bytes()) else {
        // An interior NUL means we cannot ask. Not knowing is not evidence.
        return Backing::Unknown;
    };
    // SAFETY: `buf` is a zeroed, owned `statfs`; `c_path` is a valid NUL-terminated path.
    // `statfs` only writes into `buf` and reads the path, and reports failure via `rc`.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return Backing::Unknown;
    }
    // Keep the low 32 bits so the compare is width-agnostic across arches — the same
    // treatment `store.rs`'s network-fs guard gives `f_type`.
    match (buf.f_type as i64) & 0xFFFF_FFFF {
        0x794C_7630 => Backing::Ephemeral { fs: "overlayfs" },
        0x0102_1994 => Backing::Ephemeral { fs: "tmpfs" },
        0x8584_58F6 => Backing::Ephemeral { fs: "ramfs" },
        _ => Backing::Durable,
    }
}

/// Non-Linux: a quiet no-op. The magics are not portable, and the container-without-a-
/// volume scenario this guard defends is a Linux one. The seam is the point — a platform
/// that grows its own detection (a macOS `statfs` `f_fstypename` compare, say) replaces
/// this arm and every caller is already written against [`Backing`].
#[cfg(not(target_os = "linux"))]
pub fn probe_backing(_dir: &Path) -> Backing {
    Backing::Unknown
}

/// The probe a caller injects. Production passes [`probe_backing`]; tests script the
/// answer, because the one thing a test cannot arrange portably is what filesystem it is
/// running on.
pub type BackingProbe = fn(&Path) -> Backing;

/// Settle the media store for one run — the single place either front door opens a CAS.
///
/// Both roads reach the same three states, so both must reach them the same way: `Off`
/// when the operator said so, `Disk` while persistence is active and a directory
/// resolves, and `Memory` otherwise with a warning loud enough that nobody pays a
/// provider for bytes that die at exit. It lived inside the MCP handler until the CLI
/// needed a store of its own (`kaibo deliberate` keeps a dossier, `kaibo cas` reads one),
/// and two copies of a three-state decision is how the two front doors start disagreeing
/// about what "durable" means.
///
/// Returns the store and, in disk mode, the ephemeral filesystem the directory sits on —
/// the caller surfaces that finding (`kaibo://config`'s `[cas] backing`). A disk-open
/// failure is a hard error on both roads: kaibo never invents a different path and never
/// falls back silently to memory.
///
/// `probe` is a parameter rather than a direct call so a test can drive the ephemeral
/// branch without a container.
pub fn open_media_store(
    cas: &crate::config::CasConfig,
    mode: crate::config::CasMode,
    allowed: &[&Path],
    probe: BackingProbe,
) -> Result<(Option<MediaStore>, Option<&'static str>)> {
    match mode {
        crate::config::CasMode::Off => {
            tracing::info!(
                "media CAS disabled ([cas] enabled = false) — artifact-producing tools \
                 are not advertised"
            );
            Ok((None, None))
        }
        crate::config::CasMode::Memory => {
            let why = if cas.dir.is_none() {
                "no CAS directory resolves (neither $XDG_DATA_HOME nor $HOME is set)"
            } else {
                "persistence is not active this run"
            };
            tracing::warn!(
                "MEDIA CAS IS IN-MEMORY ONLY: {why}. Generated artifacts are fetchable \
                 by digest for THIS RUN ONLY and will NOT survive a restart — they cost \
                 real provider credits and may not be reproducible. kaibo://config shows \
                 [cas] mode = \"memory\". To store artifacts durably, run with \
                 persistence enabled (and a resolvable $XDG_DATA_HOME or $HOME)."
            );
            Ok((
                Some(MediaStore::Memory(MemoryCas::new(cas.max_bytes))),
                None,
            ))
        }
        crate::config::CasMode::Disk => {
            let dir = cas
                .dir
                .clone()
                .expect("CasMode::Disk implies a resolved dir");
            let store = Cas::open(&dir, allowed, cas.max_bytes).map_err(|e| {
                CasError::Io(format!(
                    "failed to open the media CAS at {}: {e}\nkaibo never invents a \
                     different path and never falls back silently to memory. Point [cas] \
                     dir somewhere outside every allowed tree, or set [cas] enabled = \
                     false to run without it.",
                    dir.display()
                ))
            })?;
            // Disk mode's promise is "durable across restarts", and on a container with
            // no volume mounted that promise is false while everything else looks fine.
            // Ask what the directory is actually sitting on, and if the answer is a
            // filesystem that dies with the container, say so LOUDLY — then proceed
            // (Amy, 2026-07-30). Not a refusal and no ack flag: running on tmpfs on
            // purpose is legitimate, discovering it after paying for a generation is not.
            let ephemeral = probe(&dir).ephemeral_fs();
            match ephemeral {
                Some(fs) => tracing::warn!("{}", ephemeral_backing_warning(fs, &dir)),
                // Durable, or the probe could not answer. Neither is worth a word at
                // startup: one is the expected case, and the other established nothing.
                None => {
                    tracing::debug!(
                        cas_dir = %dir.display(),
                        "media CAS backing filesystem: nothing indicates it is ephemeral"
                    );
                    tracing::info!(
                        cas_dir = %dir.display(),
                        "media CAS on disk — generated artifacts are durable across restarts"
                    );
                }
            }
            Ok((Some(MediaStore::Disk(store)), ephemeral))
        }
    }
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

/// The deepest existing path at-or-above `path` — `path` itself when it exists, else
/// the nearest existing ancestor; `None` when nothing on the way up exists (an
/// absolute path always bottoms out at `/`, so this is the degenerate relative case).
/// Backs [`Cas::open`]'s structural check: whatever exists on the way to the root
/// must be a directory, or no future write can succeed.
fn first_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut cursor = path;
    loop {
        if cursor.exists() {
            return Some(cursor);
        }
        cursor = cursor.parent()?;
    }
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
