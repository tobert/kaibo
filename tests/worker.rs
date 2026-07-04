//! KaishWorker — the Send bridge to the !Send kernel. Offline & deterministic.

use std::fs;

use kaibo::sandbox::KaishWorker;
use tempfile::tempdir;

#[tokio::test]
async fn worker_reads_and_persists_across_calls() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "kai\n").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/inner.txt"), "deep\n").unwrap();

    let worker = KaishWorker::spawn(dir.path()).unwrap();

    let r = worker.run("cat hello.txt").await.unwrap();
    assert!(r.ok(), "cat should succeed: {r:?}");
    assert!(r.stdout.contains("kai"));

    // A second call reuses the same kernel — and cwd from `cd` must carry over,
    // which is the shell-continuity claim KaishWorker makes.
    let r = worker.run("cd sub").await.unwrap();
    assert!(r.ok(), "cd should succeed: {r:?}");
    let r = worker.run("pwd").await.unwrap();
    assert!(
        r.stdout.contains("sub"),
        "cwd must persist across calls, got pwd={:?}",
        r.stdout
    );
}

/// kaish 0.8.3's binary-aware `cat` returns a `Bytes` payload for a non-UTF-8 file.
/// The worker must NOT lossy-decode that to mojibake (silent corruption with exit 0);
/// it surfaces an actionable note instead — byte count plus the real outs (view_image
/// / base64). Teeth: the bytes include the UTF-8 replacement char's source (0xFF, 0x80)
/// so a regression to `text_out()` would put U+FFFD in stdout — assert stdout is empty
/// and the binary note is present.
#[tokio::test]
async fn worker_refuses_to_lossy_decode_binary_output() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("blob.bin"),
        [0xFFu8, 0xFE, 0x00, 0x80, 0x90],
    )
    .unwrap();

    let worker = KaishWorker::spawn(dir.path()).unwrap();
    let r = worker.run("cat blob.bin").await.unwrap();

    assert!(
        r.stdout.is_empty(),
        "binary output must not be lossy-decoded into stdout, got {:?}",
        r.stdout
    );
    assert!(
        r.stderr.contains("binary") && r.stderr.contains("5 bytes"),
        "must surface an actionable binary note with the byte count, got {:?}",
        r.stderr
    );
    assert!(
        r.stderr.contains("view_image") && r.stderr.contains("base64"),
        "the note must point at the real outs (view_image / base64), got {:?}",
        r.stderr
    );
    // No mojibake leaked anywhere the model reads.
    assert!(
        !r.stdout.contains('\u{FFFD}') && !r.stderr.contains('\u{FFFD}'),
        "no U+FFFD replacement chars may reach the model"
    );
}

/// The read path carries its own byte ceiling so a file swapped to something enormous
/// between a caller's `stat` and the read can't be slurped whole into memory (OOM).
/// Teeth: the file is 10× the cap, so an unbounded read (the old `project_vfs.read`)
/// would return all 4096 bytes and fail the `== cap` assertion — only a real
/// `read_range`/`File::take` ceiling returns exactly `cap`. The over-cap read must still
/// stop at the ceiling *and* signal overflow: the caller convention is `budget + 1`, and
/// a returned length past `budget` is the refuse/demote trigger.
#[tokio::test]
async fn read_file_capped_bounds_the_bytes_pulled() {
    let dir = tempdir().unwrap();
    let cap: u64 = 400;
    fs::write(dir.path().join("big.bin"), vec![0xABu8; 4096]).unwrap();

    let worker = KaishWorker::spawn(dir.path()).unwrap();
    let big = dir.path().join("big.bin");

    // A file far larger than the cap comes back truncated *at* the cap — never the
    // whole 4096 bytes. This is the OOM guard: the read stops at `cap`.
    let bytes = worker.read_file_capped(&big, cap).await.unwrap();
    assert_eq!(
        bytes.len() as u64,
        cap,
        "an over-cap file must read exactly `cap` bytes, not the whole file"
    );

    // The `budget + 1` convention: reading `budget + 1` past a `budget`-sized budget
    // yields `budget + 1` bytes for an over-budget file, and the caller reads
    // `len > budget` as overflow.
    let budget: u64 = 400;
    let probed = worker.read_file_capped(&big, budget + 1).await.unwrap();
    assert!(
        probed.len() as u64 > budget,
        "a file past the budget must return more than `budget` bytes so the caller can refuse it"
    );
}

/// A file at or under the cap reads whole — the ceiling only ever bounds an oversize
/// read, it never truncates a legitimate one. Guards against a cap that clips honest
/// files (the everyday attachment/image case).
#[tokio::test]
async fn read_file_capped_reads_small_files_whole() {
    let dir = tempdir().unwrap();
    let body = b"kai the crab\n";
    fs::write(dir.path().join("small.txt"), body).unwrap();

    let worker = KaishWorker::spawn(dir.path()).unwrap();
    // Cap far larger than the file: the whole file comes back, byte for byte.
    let bytes = worker
        .read_file_capped(dir.path().join("small.txt"), 1 << 20)
        .await
        .unwrap();
    assert_eq!(bytes, body, "an under-cap file must read whole and intact");
}

#[tokio::test]
async fn worker_denies_writes_to_real_files() {
    let dir = tempdir().unwrap();
    let victim = dir.path().join("keep.txt");
    fs::write(&victim, "keep me\n").unwrap();

    let worker = KaishWorker::spawn(dir.path()).unwrap();
    let r = worker.run("rm keep.txt").await.unwrap();

    assert!(!r.ok(), "rm must be denied through the worker too");
    assert!(victim.exists(), "the real file must survive");
}
