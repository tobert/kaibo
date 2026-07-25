//! Behavioral tests for the media CAS ([`kaibo::cas`]).
//!
//! Mirrors `tests/store.rs`'s discipline for the sibling persistence store: prove the
//! containment guard (including the symlink-teeth case), prove the write-only/no-clobber
//! shape (`create_new` — same content twice is a no-op, never a rewrite), prove the digest
//! newtype rejects anything that isn't exactly 64 lowercase hex chars *before* it can touch
//! a path, prove the soft cap refuses loudly and never evicts, and prove a round-trip is
//! byte-exact (write bytes, read them back, recompute the digest, same answer both ends).

use std::path::{Path, PathBuf};

use kaibo::cas::{Cas, CasError, Digest, Extension, Provenance};
use tempfile::TempDir;

fn prov() -> Provenance {
    Provenance {
        prompt: "a cat wearing a hat".into(),
        model: "stability/sd3.5-large".into(),
        cast: "image".into(),
        timestamp: 1_753_000_000,
        mime: "image/png".into(),
    }
}

/// A fresh CAS rooted under a temp dir plus the dir (kept alive by the caller), with a
/// generous cap so most tests don't need to think about it.
fn open(max_bytes: u64) -> (Cas, TempDir) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("cas");
    let cas = Cas::open(&root, &[], max_bytes).expect("open cas");
    (cas, dir)
}

// --- Digest newtype: validated before it ever touches a path -----------------

#[test]
fn digest_from_hex_accepts_64_lowercase_hex() {
    let hex = "e".repeat(64);
    assert!(Digest::from_hex(&hex).is_ok());
}

#[test]
fn digest_from_hex_rejects_wrong_length() {
    assert!(matches!(
        Digest::from_hex("abc"),
        Err(CasError::InvalidDigest(_))
    ));
    assert!(matches!(
        Digest::from_hex(&"a".repeat(65)),
        Err(CasError::InvalidDigest(_))
    ));
    assert!(matches!(
        Digest::from_hex(&"a".repeat(63)),
        Err(CasError::InvalidDigest(_))
    ));
}

#[test]
fn digest_from_hex_rejects_uppercase() {
    // Exactly 64 chars, but uppercase — must be refused, not silently lowercased. A model
    // handing us a digest verbatim from a provider response must match byte-for-byte.
    let hex = "A".repeat(64);
    assert!(matches!(
        Digest::from_hex(&hex),
        Err(CasError::InvalidDigest(_))
    ));
}

#[test]
fn digest_from_hex_rejects_non_hex_chars() {
    assert!(Digest::from_hex(&"g".repeat(64)).is_err());
}

/// A path-traversal-shaped string is just another invalid digest — it never gets close to
/// a path because the length/charset check runs first and unconditionally.
#[test]
fn digest_from_hex_rejects_path_traversal_shapes() {
    assert!(Digest::from_hex("../../../etc/passwd").is_err());
    assert!(Digest::from_hex("../../../../etc/passwd_pad_to_len").is_err());
}

#[test]
fn digest_of_bytes_matches_known_sha256_vector() {
    // sha256("") — the standard empty-string test vector.
    let d = Digest::of_bytes(b"");
    assert_eq!(
        d.to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn digest_round_trips_through_hex() {
    let d = Digest::of_bytes(b"hello world");
    let hex = d.to_hex();
    assert_eq!(hex.len(), 64);
    let back = Digest::from_hex(&hex).unwrap();
    assert_eq!(back.to_hex(), hex);
}

// --- Containment: mirrors SessionStore::open ---------------------------------

#[test]
fn open_refuses_path_inside_allowed_tree() {
    let project = TempDir::new().unwrap();
    let inside = project.path().join("sub/cas");
    match Cas::open(&inside, &[project.path()], u64::MAX) {
        Err(CasError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("must refuse a cas dir inside an allowed tree"),
    }
    assert!(
        !project.path().join("sub").exists(),
        "containment refusal must not create the dir it refused"
    );
}

#[cfg(unix)]
#[test]
fn open_refuses_path_reaching_into_tree_via_symlink() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join("real")).unwrap();

    let elsewhere = TempDir::new().unwrap();
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(project.path().join("real"), &link).unwrap();

    let sneaky = link.join("cas");
    match Cas::open(&sneaky, &[project.path()], u64::MAX) {
        Err(CasError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("must refuse a symlinked path that resolves inside an allowed tree"),
    }
}

#[test]
fn open_allows_path_outside_allowed_trees() {
    let project = TempDir::new().unwrap();
    let (_cas, _dir) = open(u64::MAX);
    // Sanity: opening with an unrelated allowed tree present must not spuriously refuse.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("cas");
    assert!(Cas::open(&root, &[project.path()], u64::MAX).is_ok());
}

/// A db-style equal-to-tree edge case: the cas root itself equals an allowed tree.
#[test]
fn open_refuses_path_equal_to_an_allowed_tree() {
    let project = TempDir::new().unwrap();
    match Cas::open(project.path(), &[project.path()], u64::MAX) {
        Err(CasError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("a cas dir equal to an allowed tree must be refused"),
    }
}

/// Serializes the one test that mutates the process-global cwd, and restores it on drop —
/// mirrors `tests/store.rs`'s `CwdGuard`.
struct CwdGuard {
    prev: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl CwdGuard {
    fn set(to: &Path) -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(to).unwrap();
        Self { prev, _lock: lock }
    }
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
    }
}

/// A *relative* cas dir path whose parent doesn't exist must not slip the containment
/// guard via a lexical-only fallback that stays relative (the exact `store.rs` finding).
#[test]
fn open_refuses_relative_path_that_absolutizes_into_an_allowed_tree() {
    let project = TempDir::new().unwrap();
    let canon = project.path().canonicalize().unwrap();
    let _cwd = CwdGuard::set(&canon);

    match Cas::open(Path::new("nonexistent_dir/cas"), &[&canon], u64::MAX) {
        Err(CasError::PathInAllowedTree(_)) => {}
        Err(other) => panic!("wrong error for a relative in-cwd path: {other:?}"),
        Ok(_) => panic!("a relative path resolving inside the cwd allowed tree must be refused"),
    }
    assert!(
        !canon.join("nonexistent_dir").exists(),
        "containment refusal must not create the relative parent it refused"
    );
}

// --- Write-only, create_new, no-clobber --------------------------------------

#[test]
fn put_then_get_round_trips_bytes() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"not really a png".to_vec();
    let digest = cas.put(&bytes, Extension::Png, &prov()).unwrap();
    assert_eq!(cas.get(&digest), Some(bytes.clone()));
    // The sharp edge from the design doc: the digest recomputed off the bytes that came
    // back out must equal the digest we wrote under — byte-exactness across the boundary.
    assert_eq!(Digest::of_bytes(&cas.get(&digest).unwrap()), digest);
}

#[test]
fn get_returns_none_for_unknown_digest() {
    let (cas, _d) = open(u64::MAX);
    let unknown = Digest::of_bytes(b"never written");
    assert_eq!(cas.get(&unknown), None);
    assert_eq!(cas.path_for(&unknown), None);
}

#[test]
fn path_for_uses_the_two_level_hex_shard_layout() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"shard me".to_vec();
    let digest = cas.put(&bytes, Extension::Jpeg, &prov()).unwrap();
    let hex = digest.to_hex();
    let path = cas.path_for(&digest).expect("path exists after put");

    assert!(path.ends_with(format!("{hex}.jpeg")));
    let rel = path.strip_prefix(cas.root()).unwrap();
    assert_eq!(
        rel,
        Path::new(&hex[0..2]).join(&hex[2..4]).join(format!("{hex}.jpeg"))
    );
}

#[test]
fn put_writes_a_provenance_sidecar_next_to_the_object() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"sidecar please".to_vec();
    let p = prov();
    let digest = cas.put(&bytes, Extension::Webp, &p).unwrap();

    let obj_path = cas.path_for(&digest).unwrap();
    let sidecar_path = obj_path.with_extension("json");
    assert!(sidecar_path.is_file(), "sidecar must exist next to the object");

    let raw = std::fs::read_to_string(&sidecar_path).unwrap();
    let parsed: Provenance = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.prompt, p.prompt);
    assert_eq!(parsed.model, p.model);
    assert_eq!(parsed.cast, p.cast);
    assert_eq!(parsed.timestamp, p.timestamp);
    assert_eq!(parsed.mime, p.mime);
}

/// The load-bearing no-clobber claim: writing identical content twice succeeds both times,
/// returns the same digest, and never rewrites the file on disk (create_new / O_EXCL —
/// `AlreadyExists` is mapped to `Ok`, not treated as a failure or a truncate-and-rewrite).
#[test]
fn writing_the_same_content_twice_is_a_noop_not_a_rewrite() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"idempotent bytes".to_vec();

    let d1 = cas.put(&bytes, Extension::Png, &prov()).unwrap();
    let path = cas.path_for(&d1).unwrap();
    let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

    // Second write of the exact same bytes must succeed, return the same digest, and must
    // not disturb the file already on disk.
    std::thread::sleep(std::time::Duration::from_millis(10));
    let d2 = cas.put(&bytes, Extension::Png, &prov()).unwrap();
    assert_eq!(d1, d2, "same content hashes to the same digest");
    let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(mtime1, mtime2, "an existing object must never be rewritten");
    assert_eq!(cas.get(&d1).unwrap(), bytes);
}

// --- Soft cap: refuse loudly, never evict ------------------------------------

#[test]
fn soft_cap_refuses_a_write_that_would_exceed_it() {
    let (cas, _d) = open(16); // 16 bytes total budget
    let small = vec![1u8; 8];
    let d1 = cas.put(&small, Extension::Png, &prov()).unwrap();

    let too_big = vec![2u8; 100];
    match cas.put(&too_big, Extension::Png, &prov()) {
        Err(CasError::CapacityExceeded { .. }) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("a write past the soft cap must be refused"),
    }

    // Never evicts: the first object must still be present and readable.
    assert_eq!(cas.get(&d1), Some(small));
    // The refused content must never have landed on disk.
    let rejected_digest = Digest::of_bytes(&too_big);
    assert_eq!(cas.get(&rejected_digest), None);
}

/// A dedup write of content already on disk must not be penalized by the cap check just
/// because a naive "current + incoming" sum would look like it exceeds — it adds no bytes.
#[test]
fn soft_cap_does_not_block_a_dedup_write_of_existing_content() {
    let bytes = vec![7u8; 10];
    let (cas, _d) = open(10); // exactly the size of one object, zero slack
    let d1 = cas.put(&bytes, Extension::Gif, &prov()).unwrap();
    // Re-putting the identical bytes must still succeed even though the cap has no slack
    // left for "new" bytes — it isn't new, it's the same object.
    let d2 = cas.put(&bytes, Extension::Gif, &prov()).unwrap();
    assert_eq!(d1, d2);
}
