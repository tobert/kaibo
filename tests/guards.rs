//! Operational guards for the caller-facing `run_kaish`: a per-exec wall-clock
//! timeout and an output cap. The kernel-level read-only boundary (tests/sandbox.rs)
//! is necessary but not sufficient once a caller drives the shell with no
//! `max_turns` braking it — a slow script could wedge the serial worker, and a
//! `cat` of a huge file could flood the caller's context. These pin both brakes.
//!
//! Kernel-level, like tests/sandbox.rs: current-thread runtime, `run` directly.

use std::fs;
use std::time::{Duration, Instant};

use kaibo::sandbox::{build_readonly_kernel, build_readonly_kernel_with_timeout, run};
use tempfile::tempdir;

/// A zero timeout makes the kernel return 124 immediately without spawning —
/// the most deterministic possible proof that the budget is actually threaded
/// into the kernel config (not silently dropped).
#[tokio::test(flavor = "current_thread")]
async fn zero_timeout_yields_124_without_running() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "hi\n").unwrap();

    let kernel = build_readonly_kernel_with_timeout(dir.path(), Duration::ZERO).unwrap();
    let r = run(&kernel, "cat f.txt").await.unwrap();

    assert_eq!(r.code, 124, "a zero budget must time out (124), got {r:?}");
}

/// A genuinely slow script is killed at the budget, not run to completion: the
/// brake fires on elapsed wall-clock, returning 124 well before the sleep ends.
#[tokio::test(flavor = "current_thread")]
async fn a_slow_script_is_killed_at_the_budget() {
    let dir = tempdir().unwrap();

    let kernel =
        build_readonly_kernel_with_timeout(dir.path(), Duration::from_millis(200)).unwrap();
    let started = Instant::now();
    let r = run(&kernel, "sleep 10").await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(r.code, 124, "a slow script must be killed with 124, got {r:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the timeout must fire fast, not run the full sleep — took {elapsed:?}"
    );
}

/// A read that exceeds the 8 KB MCP output cap must not hand the caller the whole
/// file: the result is bounded (truncated) — or, if kaish's spill path can't write
/// in the read-only sandbox, it surfaces a marker instead of the raw flood. Either
/// way the caller's context is protected. Teeth: the source is 8x the cap.
#[tokio::test(flavor = "current_thread")]
async fn oversized_output_is_capped_or_marked() {
    let dir = tempdir().unwrap();
    // 64 KB of content — well past the 8 KB cap KernelConfig::mcp() installs.
    let big = "x".repeat(64 * 1024);
    fs::write(dir.path().join("big.txt"), &big).unwrap();

    let kernel = build_readonly_kernel(dir.path()).unwrap();
    let r = run(&kernel, "cat big.txt").await.unwrap();

    let out = r.text_out();
    let total = out.len() + r.err.len();
    assert!(
        total < big.len(),
        "oversized output must be capped, not echoed whole \
         (got {total} bytes from a {} byte source, code={})",
        big.len(),
        r.code
    );
    assert!(
        out.contains("output truncated"),
        "a truncated read must carry a marker so the caller knows it's partial, got tail: {:?}",
        &out[out.len().saturating_sub(120)..]
    );
    // KNOWN IMPERFECTION (tracked in kaish's issues.md): under the `localfs`
    // feature the cap spills the full output to a real host file in the XDG
    // runtime dir and remaps the exit code to 3. kaibo's invariant (never modify
    // the *project*) holds — the project mount is read-only — but the ideal is
    // in-memory head+tail truncation with no host write and the real code preserved.
}
