//! The sandbox safety boundary — kaibo's single most important invariant.
//!
//! These run on a current-thread runtime because the kaish kernel's `execute`
//! returns a `!Send` future (it's fine to `.await` directly without spawning).

use std::fs;

use kaibo::sandbox::{build_readonly_kernel, run};
use tempfile::tempdir;

/// Reads through the sandbox work and see the live project tree.
#[tokio::test(flavor = "current_thread")]
async fn reads_see_real_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "kai the crab\n").unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "cat hello.txt").await.unwrap();

    assert!(r.ok(), "cat should succeed, got code={} err={:?}", r.code, r.err);
    assert!(
        r.text_out().contains("kai the crab"),
        "cat should return file contents, got: {:?}",
        r.text_out()
    );
}

/// Deleting a real project file must fail, and the file must survive.
#[tokio::test(flavor = "current_thread")]
async fn rm_on_project_file_is_denied_and_file_survives() {
    let dir = tempdir().unwrap();
    let victim = dir.path().join("important.txt");
    fs::write(&victim, "do not delete me\n").unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "rm important.txt").await.unwrap();

    assert!(!r.ok(), "rm must fail against a read-only mount");
    assert!(
        victim.exists(),
        "the real file must still exist after a denied rm"
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "do not delete me\n");
}

/// Writing/redirecting into the project tree must not create a real file.
#[tokio::test(flavor = "current_thread")]
async fn writes_into_project_do_not_touch_disk() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let _ = run(&kernel, "echo pwned > newfile.txt").await.unwrap();

    assert!(
        !dir.path().join("newfile.txt").exists(),
        "a redirect into the read-only project must not create a real file"
    );
}

/// External commands are disabled — the explorer can't escape kaish.
#[tokio::test(flavor = "current_thread")]
async fn external_commands_are_disabled() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    // `/bin/sh` is not a kaish builtin; with external commands off it must fail.
    let r = run(&kernel, "/bin/sh -c 'echo escaped'").await.unwrap();
    assert!(
        !r.ok(),
        "external command should be refused, got code={} out={:?}",
        r.code,
        r.text_out()
    );
}

/// `touch` on a *new* file is already stopped by the read-only mount.
#[tokio::test(flavor = "current_thread")]
async fn touch_new_file_is_denied() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let r = run(&kernel, "touch sneaky.txt").await.unwrap();
    assert!(!r.ok(), "touch must be denied, got code={}", r.code);
    assert!(
        !dir.path().join("sneaky.txt").exists(),
        "touch must not create a real file"
    );
}

/// `touch` on an *existing* file takes the `std::fs` mtime path that bypasses the
/// backend mount — only the denylist stops it. Teeth: the real mtime must not move.
#[tokio::test(flavor = "current_thread")]
async fn touch_existing_file_cannot_bump_mtime() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("real.txt");
    fs::write(&target, "x\n").unwrap();
    let before = fs::metadata(&target).unwrap().modified().unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "touch real.txt").await.unwrap();

    assert!(!r.ok(), "touch on existing file must be denied, code={}", r.code);
    assert!(
        r.err.contains("read-only sandbox"),
        "the denylist (not the mount) should catch the std::fs mtime path, got err={:?}",
        r.err
    );
    let after = fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(before, after, "the real file's mtime must not change");
}

/// `git` writes the real `.git` via libgit2, bypassing the mount entirely.
#[tokio::test(flavor = "current_thread")]
async fn git_is_blocked_and_inits_no_repo() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let r = run(&kernel, "git init").await.unwrap();
    assert!(!r.ok(), "git must be denied, got code={}", r.code);
    assert!(
        !dir.path().join(".git").exists(),
        "git init must not create a real .git directory"
    );
}

/// `spawn` would launch a real external process — the escape hatch the
/// external-command flag doesn't cover. It must be denied.
#[tokio::test(flavor = "current_thread")]
async fn spawn_is_blocked() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let r = run(&kernel, "spawn /bin/echo escaped").await.unwrap();
    assert!(!r.ok(), "spawn must be denied, got code={}", r.code);
}
