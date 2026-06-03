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
