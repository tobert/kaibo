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
