//! The sandbox safety boundary — kaibo's single most important invariant.
//!
//! These run on a current-thread runtime because the kaish kernel's `execute`
//! returns a `!Send` future (it's fine to `.await` directly without spawning).

use std::fs;

use kaibo::sandbox::{SandboxConfig, build_readonly_kernel, build_readonly_kernel_with, run};
use tempfile::tempdir;

/// Reads through the sandbox work and see the live project tree.
#[tokio::test(flavor = "current_thread")]
async fn reads_see_real_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "kai the crab\n").unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "cat hello.txt").await.unwrap();

    assert!(
        r.ok(),
        "cat should succeed, got code={} err={:?}",
        r.code,
        r.err
    );
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

/// Redirecting into the project tree must not create a real file. The target is an
/// ABSOLUTE path into the project so the redirect resolves to the read-only LocalFs
/// mount deterministically. (As of kaish 0.8.1 a bare relative `> f` resolves against
/// cwd — here the project root — so it would also be refused as read-only; earlier
/// kaish resolved relative targets against `/` and they landed in scratch instead.
/// The absolute path pins the mount under test regardless.) Teeth: mount the project
/// writable (`LocalFs::new`) and this fails — the read-only mount is the only thing
/// refusing the write. The path comes from a `tempdir` (host-independent); we give
/// it a non-dotted prefix so no path component trips kaish's dot-filename
/// mis-tokenization.
#[tokio::test(flavor = "current_thread")]
async fn writes_into_project_do_not_touch_disk() {
    let dir = tempfile::Builder::new()
        .prefix("kaibo-ro")
        .tempdir()
        .unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let target = dir.path().join("newfile.txt");
    let script = format!("echo pwned > {}", target.display());
    let r = run(&kernel, &script).await.unwrap();

    assert!(
        !r.ok(),
        "a redirect into the read-only project must be refused, got {r:?}"
    );
    assert!(
        r.err.contains("read-only"),
        "the refusal should cite the read-only mount, got {r:?}"
    );
    assert!(
        !target.exists(),
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

/// `touch` on an *existing* file bumps its mtime — the path that used to escape via
/// raw `std::fs` and bypass the mount. The upstream fix routes that bump through the
/// backend's `set_mtime`, which the read-only mount rejects, so kaibo no longer
/// needs a shadow-block. Teeth: the refusal cites the read-only filesystem AND the
/// real mtime must not move (a regression in the upstream routing would move it).
#[tokio::test(flavor = "current_thread")]
async fn touch_existing_file_cannot_bump_mtime() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("real.txt");
    fs::write(&target, "x\n").unwrap();
    let before = fs::metadata(&target).unwrap().modified().unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "touch real.txt").await.unwrap();

    assert!(
        !r.ok(),
        "touch on existing file must be denied, code={}",
        r.code
    );
    assert!(
        r.err.contains("read-only"),
        "the read-only mount should reject the mtime bump, got err={:?}",
        r.err
    );
    let after = fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(before, after, "the real file's mtime must not change");
}

/// `mktemp` used to create a *real* temp file via `std::fs`. The upstream fix
/// resolves its parent dir through the VFS, so in kaibo's sandbox it lands in the
/// ephemeral `MemoryFs` mounted at `/` (there is no writable real mount), never on
/// the host's `/tmp`. Teeth: whatever path it hands back must NOT exist on the real
/// filesystem — if the resolution ever fell back to host `std::fs`, it would.
#[tokio::test(flavor = "current_thread")]
async fn mktemp_lands_in_memory_not_real_disk() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let r = run(&kernel, "mktemp").await.unwrap();
    assert!(
        r.ok(),
        "mktemp should succeed into ephemeral memory, got {r:?}"
    );

    let out = r.text_out();
    let path = out.trim();
    assert!(
        !path.is_empty(),
        "mktemp should print the temp path it created, got {r:?}"
    );
    assert!(
        !std::path::Path::new(path).exists(),
        "mktemp must not create a file on the real filesystem, but {path} exists"
    );
}

/// `git` would write a real `.git` via libgit2, bypassing the mount entirely — but
/// it lives on the `git` axis, which kaibo doesn't compile, so it isn't even a
/// registered builtin. Pin that it stays gone: `git init` is refused and no repo
/// appears (a regression that pulled the axis in would make a real `.git`).
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

/// `spawn` would launch a real external process — but it lives on the `subprocess`
/// axis, which kaibo doesn't compile, so it isn't a registered builtin. It must be
/// refused (a regression that enabled the axis would let it run).
#[tokio::test(flavor = "current_thread")]
async fn spawn_is_blocked() {
    let dir = tempdir().unwrap();
    let kernel = build_readonly_kernel(dir.path()).unwrap();

    let r = run(&kernel, "spawn /bin/echo escaped").await.unwrap();
    assert!(!r.ok(), "spawn must be denied, got code={}", r.code);
}

/// The exit-code contract a caller-facing `run_kaish` advertises: a *shadow block*
/// is exit 126, distinguishable from an ordinary *script failure*. An automated
/// caller keys off this to tell "the sandbox refused you" from "your command
/// failed", so pin that 126 is not just "any non-zero". The read-only invariant
/// itself is now structural — a denied write surfaces as the VFS permission-denied
/// code (see the `touch`/`rm` tests) — so we drive the 126 path through the one
/// mechanism that still emits it: a config-disabled builtin.
#[tokio::test(flavor = "current_thread")]
async fn shadow_block_is_126_and_distinct_from_a_plain_failure() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "hi\n").unwrap();

    // Disable an otherwise-working builtin so the shadow (Blocked → 126) fires.
    let sandbox = SandboxConfig {
        disable_builtins: vec!["cat".to_string()],
        ..SandboxConfig::default()
    };
    let kernel = build_readonly_kernel_with(dir.path(), &sandbox).unwrap();

    // The shadow-blocked builtin → 126 + the sandbox marker.
    let blocked = run(&kernel, "cat f.txt").await.unwrap();
    assert_eq!(
        blocked.code, 126,
        "a sandbox block must be exit 126, got {blocked:?}"
    );
    assert!(
        blocked.err.contains("read-only sandbox")
            || blocked.text_out().contains("read-only sandbox"),
        "the 126 must carry the sandbox marker (126 also means POSIX not-executable), got {blocked:?}"
    );

    // An ordinary failure (a still-enabled builtin on a missing file) → non-zero
    // but NOT 126, so it can't be mistaken for a block.
    let failed = run(&kernel, "grep needle does-not-exist.txt")
        .await
        .unwrap();
    assert!(
        !failed.ok(),
        "grep on a missing file must fail, got {failed:?}"
    );
    assert_ne!(
        failed.code, 126,
        "a plain failure must not collide with the 126 sandbox-block code, got {failed:?}"
    );
}

/// The `/` scratch MemoryFs is bounded: a redirect that writes past the configured
/// `scratch_limit_bytes` fails loudly (ENOSPC-style) instead of eating host RAM for
/// the kernel's whole lifetime — the unbounded-scratch surprise tracked in
/// `docs/issues.md`. The target is an ABSOLUTE path outside the project mount, so the
/// router sends it to the `/` MemoryFs (the budgeted mount) rather than the read-only
/// project mount; a bare relative target would now resolve against cwd (the project)
/// and be refused as read-only instead. Teeth: the generous default budget below
/// swallows the same write, so it's the cap — not some other refusal — doing the work.
#[tokio::test(flavor = "current_thread")]
async fn scratch_writes_past_the_budget_are_refused() {
    let dir = tempfile::Builder::new()
        .prefix("kaibo-ro")
        .tempdir()
        .unwrap();

    // A tiny budget, then a redirect that overruns it in one write.
    let tight = SandboxConfig {
        scratch_limit_bytes: 16,
        ..SandboxConfig::default()
    };
    let kernel = build_readonly_kernel_with(dir.path(), &tight).unwrap();
    let overrun = "echo this-line-is-well-over-sixteen-bytes-long > /scratch-grow";
    let r = run(&kernel, overrun).await.unwrap();
    assert!(
        !r.ok(),
        "a write past the scratch budget must be refused, got {r:?}"
    );
    assert!(
        r.err.contains("budget") || r.text_out().contains("budget"),
        "the refusal should cite the exhausted scratch budget, got {r:?}"
    );

    // Teeth: the same write under the generous default budget succeeds — so the
    // refusal above was the cap, not a redirect or read-only artifact.
    let roomy = build_readonly_kernel_with(dir.path(), &SandboxConfig::default()).unwrap();
    let r = run(&roomy, overrun).await.unwrap();
    assert!(
        r.ok(),
        "the same write must succeed under the default budget, got {r:?}"
    );
}

/// The builtin-schema snapshot that drives kaibo's help surface must enumerate the
/// real read tools (so `help builtins` and `kaibo://kaish/builtins` aren't empty)
/// and the `help` builtin itself (so an agent can `help syntax` inside run_kaish).
#[test]
fn builtin_schemas_enumerate_the_read_toolbox() {
    let names: Vec<String> = kaibo::sandbox::builtin_schemas()
        .expect("schema kernel must build")
        .into_iter()
        .map(|s| s.name)
        .collect();
    for want in ["cat", "grep", "find", "help"] {
        assert!(
            names.iter().any(|n| n == want),
            "builtin_schemas must list {want:?}, got {names:?}"
        );
    }
}
