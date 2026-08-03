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
        seed: Some("9259671".into()),
    }
}

/// A sidecar written before `seed` existed must still deserialize, because the CAS
/// **cannot rewrite it** — there is no migration pass available when every write is
/// `create_new` and nothing is ever deleted. A new *required* field would silently turn a
/// user's paid-for archive into unreadable bytes.
///
/// Be precise about what this pins, because it is easy to overclaim: serde already treats
/// an `Option` field as optional, so this does **not** discriminate a redundant
/// `#[serde(default)]` — verified by removing the attribute and watching this still pass.
/// What it does catch is the realistic future mistake: making `seed` (or any later field)
/// **non-`Option`**, which fails this file at compile time on the `None` comparison below.
/// That is a real guard, just a narrower one than "the attribute is present."
#[test]
fn a_sidecar_written_before_seed_existed_still_deserializes() {
    let old = r#"{"prompt":"a cat","model":"m","cast":"c","timestamp":1,"mime":"image/png"}"#;
    let p: Provenance = serde_json::from_str(old).expect("an older sidecar must still load");
    assert_eq!(p.seed, None, "a missing seed reads as None, not an error");
    assert_eq!(p.prompt, "a cat");
}

/// The round trip a user's `jq`-based GC depends on: what we write is what we read back,
/// seed included.
#[test]
fn provenance_round_trips_through_json_with_its_seed() {
    let json = serde_json::to_string(&prov()).expect("serialize");
    let back: Provenance = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, prov());
    assert_eq!(back.seed.as_deref(), Some("9259671"));
}

/// A fresh CAS rooted under a temp dir plus the dir (kept alive by the caller), capped
/// at `max_bytes` — the tests that actually exercise the soft cap.
fn open(max_bytes: u64) -> (Cas, TempDir) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("cas");
    let cas = Cas::open(&root, &[], Some(max_bytes)).expect("open cas");
    (cas, dir)
}

/// A fresh UNCAPPED CAS — the default posture, and what most tests want: no cap means
/// no size accounting at all, so they never have to think about a budget.
fn open_uncapped() -> (Cas, TempDir) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("cas");
    let cas = Cas::open(&root, &[], None).expect("open cas");
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
    match Cas::open(&inside, &[project.path()], None) {
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
    match Cas::open(&sneaky, &[project.path()], None) {
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
    assert!(Cas::open(&root, &[project.path()], None).is_ok());
}

/// A db-style equal-to-tree edge case: the cas root itself equals an allowed tree.
#[test]
fn open_refuses_path_equal_to_an_allowed_tree() {
    let project = TempDir::new().unwrap();
    match Cas::open(project.path(), &[project.path()], None) {
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

    match Cas::open(Path::new("nonexistent_dir/cas"), &[&canon], None) {
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
    assert_eq!(cas.get(&digest).unwrap(), Some(bytes.clone()));
    // The sharp edge from the design doc: the digest recomputed off the bytes that came
    // back out must equal the digest we wrote under — byte-exactness across the boundary.
    assert_eq!(
        Digest::of_bytes(&cas.get(&digest).unwrap().unwrap()),
        digest
    );
}

#[test]
fn get_returns_none_for_unknown_digest() {
    let (cas, _d) = open(u64::MAX);
    let unknown = Digest::of_bytes(b"never written");
    assert_eq!(cas.get(&unknown).unwrap(), None);
    assert_eq!(cas.path_for(&unknown), None);
}

// --- Integrity: get/put verify the digest before trusting bytes --------------

/// The defect this guards: `write_new_file` has no fsync, so a crash between `open` and a
/// completed `write_all` can leave a truncated/wrong-content file sitting at an object's
/// content-addressed path. Nothing on the write side can detect that after the fact — the
/// path existing was being treated as proof the bytes were good. Simulate exactly that by
/// writing bad bytes directly at the path a real `put` would have used, then prove `get`
/// notices rather than handing back silently-wrong bytes.
#[test]
fn get_returns_corrupt_error_for_object_whose_content_does_not_match_its_digest() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"the real content".to_vec();
    let digest = cas.put(&bytes, Extension::Png, &prov()).unwrap();
    let path = cas.path_for(&digest).unwrap();

    // Simulate a crash mid-write: truncated/wrong bytes at the exact path `put` used.
    std::fs::write(&path, b"truncated garbage").unwrap();

    match cas.get(&digest) {
        Err(CasError::Corrupt { .. }) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(v) => panic!("must not silently return unverified bytes, got: {v:?}"),
    }
}

/// `put` must not treat a poisoned dedup slot as success: on `AlreadyExists`, it must read
/// back the existing object and verify it hashes to the digest before reporting `Ok`. A
/// caller that thinks its bytes were saved when a poisoned slot silently ate them is the
/// worst outcome here — so this must be a loud `Err(Corrupt)`, never `Ok`.
#[test]
fn put_returns_corrupt_error_when_dedup_slot_is_poisoned() {
    let (cas, _d) = open(u64::MAX);
    let bytes = b"good bytes that should be saved".to_vec();
    let digest = Digest::of_bytes(&bytes);

    // Poison the slot out-of-band, as if a prior crash left a truncated file there — before
    // any real `put` ever wrote to it.
    let shard = cas.root().join(&digest.to_hex()[0..2]).join(&digest.to_hex()[2..4]);
    std::fs::create_dir_all(&shard).unwrap();
    let path = shard.join(format!("{}.png", digest.to_hex()));
    std::fs::write(&path, b"poisoned").unwrap();

    match cas.put(&bytes, Extension::Png, &prov()) {
        Err(CasError::Corrupt { .. }) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(d) => panic!(
            "must not silently report success over a poisoned slot, got Ok({})",
            d.to_hex()
        ),
    }
}

/// An object that exists but cannot be read at all (not just wrong content) must surface an
/// error, never fold into `Ok(None)` — that would look identical to "never written" and hide
/// a real filesystem problem. Skipped rather than silently passing if this process can read
/// anything regardless of permissions (e.g. running as root).
#[cfg(unix)]
#[test]
fn get_surfaces_io_error_for_unreadable_existing_object() {
    use std::os::unix::fs::PermissionsExt;

    let (cas, _d) = open(u64::MAX);
    let bytes = b"unreadable please".to_vec();
    let digest = cas.put(&bytes, Extension::Jpeg, &prov()).unwrap();
    let path = cas.path_for(&digest).unwrap();

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Portable root/override detection: rather than pulling in a uid-checking dependency,
    // just ask whether *reading the file directly* ignores the 0o000 bits (root, or a
    // capability/ACL override, does exactly this). If it does, the whole premise of this
    // test doesn't hold in this environment — skip rather than silently pass on a
    // conclusion (`Ok`) the test never actually reached. Must be checked before restoring
    // permissions below, and before calling `cas.get` so a false pass can't hide behind it.
    let can_read_anyway = std::fs::read(&path).is_ok();

    let result = cas.get(&digest);
    // Restore permissions so `TempDir`'s drop can clean up the file afterward.
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));

    if can_read_anyway {
        eprintln!(
            "skipping: this process can read a 0o000 file directly (likely running as root) — \
             cannot exercise an unreadable-existing-object condition here"
        );
        return;
    }

    match result {
        Err(CasError::Io(_)) => {}
        other => panic!("expected Err(Io(_)) for an unreadable existing object, got {other:?}"),
    }
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
    assert_eq!(cas.get(&d1).unwrap().unwrap(), bytes);
}

// --- Soft cap: refuse loudly, never evict ------------------------------------

/// The cap is OPT-IN, and an uncapped store never measures itself.
///
/// This is the load-bearing property, not a convenience: enforcing a cap means summing
/// every file in the store, and `put` does that on the write path — for every new object,
/// forever, over a store that by design never deletes anything. Two-level sharding means
/// that walk is up to 65,536 `readdir`s plus a `metadata` per object, and it only gets
/// slower as the store fills. So an operator who sets no cap pays none of it. The proof
/// here is behavioral: with the store's own size accounting made impossible (the shard
/// tree is replaced by a file, so any attempt to walk it errors), an uncapped `put` still
/// succeeds — it never looked.
#[test]
#[cfg(unix)]
fn an_uncapped_cas_never_walks_the_store_to_size_it() {
    use std::os::unix::fs::PermissionsExt;

    let (cas, _d) = open_uncapped();
    let first = cas
        .put(&[1u8; 32], Extension::Png, &prov())
        .expect("first put");
    assert!(cas.get(&first).unwrap().is_some());

    // Sabotage the size walk WITHOUT touching the shard any write needs: a sibling
    // top-level shard directory that exists but cannot be read. `read_dir` on it fails
    // with EACCES, so summing the store is impossible — while `create_dir_all` and the
    // object write, which only ever touch their own digest's shard, are unaffected.
    let root = cas.root().to_path_buf();
    let unreadable = root.join("zz");
    std::fs::create_dir_all(&unreadable).expect("create a sibling shard");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
        .expect("make it unreadable");

    // An uncapped put must not care, because it must never take the sizing path at all.
    let second = cas
        .put(&[2u8; 32], Extension::Png, &prov())
        .expect("an uncapped put must not depend on being able to size the store");
    assert_eq!(cas.get(&second).unwrap().unwrap(), vec![2u8; 32]);

    // Prove the sabotage actually bites, so the success above means something: the SAME
    // root opened WITH a cap must fail trying to walk it. Without this the test would
    // still pass if `put` had simply stopped enforcing caps, or if the sabotage had
    // quietly done nothing.
    //
    // The precondition is checked directly rather than by testing for uid 0: root (and
    // anything holding CAP_DAC_OVERRIDE, or a filesystem mounted without permission
    // enforcement) can still read the directory, and in that case there is no sabotage to
    // observe. Asking whether the read actually fails is exact, needs no libc — which is
    // a Linux-only dependency in this tree — and stays honest under every such case.
    if std::fs::read_dir(&unreadable).is_err() {
        let capped = Cas::open(&root, &[], Some(1 << 30)).expect("open the same root capped");
        match capped.put(&[3u8; 32], Extension::Png, &prov()) {
            Err(CasError::Io(msg)) => assert!(
                msg.contains("scanning CAS size"),
                "a capped put must fail IN the size walk, got: {msg}"
            ),
            other => panic!("the sabotage must break a capped put, got: {other:?}"),
        }
    }

    // Leave the tree removable by the TempDir drop.
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).ok();
}

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
    assert_eq!(cas.get(&d1).unwrap(), Some(small));
    // The refused content must never have landed on disk.
    let rejected_digest = Digest::of_bytes(&too_big);
    assert_eq!(cas.get(&rejected_digest).unwrap(), None);
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

// --- MemoryCas: same contract, no filesystem ---------------------------------

use kaibo::cas::{MediaStore, MemoryCas};

/// The in-memory store round-trips bytes by digest, with the extension (and thus
/// the mime a resource read serves) intact — the degraded-mode contract for a run
/// without persistence.
#[test]
fn memory_cas_put_then_get_round_trips_bytes_and_extension() {
    let mem = MemoryCas::new(None);
    let bytes = b"pretend-png".to_vec();
    let digest = mem.put(&bytes, Extension::Png, &prov()).unwrap();
    assert_eq!(digest, Digest::of_bytes(&bytes));
    assert_eq!(mem.get(&digest), Some((bytes, Extension::Png)));
    assert_eq!(mem.provenance(&digest).unwrap(), prov());
}

#[test]
fn memory_cas_get_returns_none_for_unknown_digest() {
    let mem = MemoryCas::new(None);
    let missing = Digest::of_bytes(b"never stored");
    assert_eq!(mem.get(&missing), None);
}

/// Identical content twice is one object and one digest — dedup, not accumulation.
#[test]
fn memory_cas_dedup_returns_the_same_digest() {
    let mem = MemoryCas::new(None);
    let d1 = mem.put(b"same", Extension::Webp, &prov()).unwrap();
    let d2 = mem.put(b"same", Extension::Webp, &prov()).unwrap();
    assert_eq!(d1, d2);
}

/// The soft cap keeps its meaning in memory: refuse loudly, never evict — and a
/// dedup write of held content is exempt, exactly as on disk.
#[test]
fn memory_cas_cap_refuses_loudly_and_never_evicts() {
    let mem = MemoryCas::new(Some(10));
    let first = vec![1u8; 8];
    let d1 = mem.put(&first, Extension::Png, &prov()).unwrap();
    match mem.put(&[2u8; 100], Extension::Png, &prov()) {
        Err(CasError::CapacityExceeded { max_bytes: 10, .. }) => {}
        other => panic!("a write past the cap must be refused, got {other:?}"),
    }
    // Never evicts, and the dedup write of the held object still succeeds with no slack.
    assert_eq!(mem.get(&d1).map(|(b, _)| b), Some(first.clone()));
    assert_eq!(mem.put(&first, Extension::Png, &prov()).unwrap(), d1);
}

// --- MediaStore: one seam over both modes ------------------------------------

/// Disk mode exposes the real filesystem path beside the digest; memory mode has no
/// path at all (the kaibo://cas resource is its only retrieval channel). Both modes
/// serve the same bytes+extension read.
#[test]
fn media_store_paths_exist_on_disk_and_not_in_memory() {
    let (cas, _dir) = open_uncapped();
    let disk = MediaStore::Disk(cas);
    let mem = MediaStore::Memory(MemoryCas::new(None));

    let bytes = b"artifact".to_vec();
    let d_disk = disk.put(&bytes, Extension::Jpeg, &prov()).unwrap();
    let d_mem = mem.put(&bytes, Extension::Jpeg, &prov()).unwrap();
    assert_eq!(d_disk, d_mem, "the address is the content, mode-independent");

    assert!(disk.path_for(&d_disk).is_some_and(|p| p.is_file()));
    assert!(disk.root().is_some());
    assert_eq!(disk.mode(), "disk");

    assert_eq!(mem.path_for(&d_mem), None);
    assert_eq!(mem.root(), None);
    assert_eq!(mem.mode(), "memory");

    assert_eq!(
        disk.get(&d_disk).unwrap(),
        Some((bytes.clone(), Extension::Jpeg))
    );
    assert_eq!(mem.get(&d_mem).unwrap(), Some((bytes, Extension::Jpeg)));
}

// --- Extension <-> mime -------------------------------------------------------

/// The wire-mime mapping is closed and case/parameter-tolerant on parse (RFC 7231),
/// canonical on render — and refuses anything the CAS cannot name on disk.
#[test]
fn extension_maps_mimes_both_ways_and_refuses_unknown() {
    for ext in Extension::ALL {
        assert_eq!(Extension::from_mime(ext.mime()), Some(ext));
    }
    assert_eq!(Extension::from_mime("IMAGE/PNG; charset=binary"), Some(Extension::Png));
    assert_eq!(Extension::from_mime("audio/mpeg"), None);
    assert_eq!(Extension::from_mime("model/gltf-binary"), None);
}
