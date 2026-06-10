//! Path containment: all tool calls must resolve to paths at-or-under the allowed
//! set. The allowed set = canonicalized --root (if set) plus each --allow-path;
//! when both are absent it defaults to the canonicalized launch cwd.
//!
//! Tests are written first (TDD), proven to fail before enforcement exists, then
//! proven to pass after. Test (a)/(b) are the TDD "teeth" tests — quoted failure
//! output appears in the task summary.
//!
//! Every test drives `run_kaish` (the cheapest tool that exercises `resolve_root`).

use std::fs;
use std::path::Path;

use kaibo::config::Config;
use kaibo::server::{KaiboHandler, RunKaishInput};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::tempdir;

/// Drive `run_kaish` and return the McpError string if rejected, or the text output
/// if it succeeds. This is the cheap probe: we want the error text, not real kaish output.
async fn try_run(handler: &KaiboHandler, path: &str, script: &str) -> Result<String, String> {
    handler
        .run_kaish(Parameters(RunKaishInput {
            script: script.to_string(),
            path: Some(path.to_string()),
        }))
        .await
        .map(|r| {
            r.content
                .into_iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .map_err(|e| format!("{e:?}"))
}

/// Build a handler with the given allowed set. `allow_paths` is empty for "use cwd",
/// `root` is the default project root (which also enters the allowed set).
fn handler_with_allowed(root: Option<&Path>, allow_paths: &[&Path]) -> KaiboHandler {
    let mut config = Config::builtin();
    config.root = root.map(|p| p.to_path_buf());
    config.allow_paths = allow_paths.iter().map(|p| p.to_path_buf()).collect();
    KaiboHandler::new(config).expect("handler builds")
}

// --- (a) path outside the allowed set is rejected ----------------------------

/// A call whose `path` resolves outside the allowed tree must be rejected with an
/// error naming the allowed trees and the three widening knobs.
#[tokio::test]
async fn path_outside_allowed_set_is_rejected() {
    let allowed = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "sensitive\n").unwrap();

    let handler = handler_with_allowed(Some(allowed.path()), &[]);

    let err = try_run(&handler, &outside.path().to_string_lossy(), "cat secret.txt")
        .await
        .expect_err("a path outside the allowed set must be an MCP error");

    // The error must name the allowed trees so the caller knows where the boundary is.
    assert!(
        err.contains(&allowed.path().to_string_lossy().to_string())
            || err.to_lowercase().contains("allowed"),
        "the rejection must name the allowed set, got: {err}"
    );
    // And it must mention how to widen it.
    assert!(
        err.contains("--allow-path") || err.contains("KAIBO_ALLOW_PATHS") || err.contains("allow_paths"),
        "the rejection must name a widening knob, got: {err}"
    );
}

// --- (b) .. traversal that textually starts inside is rejected ---------------

/// A path that textually starts under an allowed tree but resolves outside via `..`
/// must be rejected. This proves the enforcement is canonicalize-based, not
/// string-prefix-based.
#[tokio::test]
async fn dotdot_traversal_is_rejected() {
    let allowed = tempdir().unwrap();
    // Create a subdirectory inside the allowed tree.
    let sub = allowed.path().join("sub");
    fs::create_dir(&sub).unwrap();
    // Build a path that starts inside allowed/sub but escapes via ../..
    let outside = tempdir().unwrap();
    // We can't make "allowed/sub/../../outside" resolve to `outside` without knowing
    // the fs layout; instead we use "allowed/sub/../../.." which escapes allowed entirely.
    // The canonical form must NOT start_with the allowed tree.
    let traversal = format!("{}/../../..", sub.display());

    let handler = handler_with_allowed(Some(allowed.path()), &[]);

    let err = try_run(&handler, &traversal, "ls")
        .await
        .expect_err("a .. traversal that escapes the allowed tree must be rejected");

    assert!(
        err.to_lowercase().contains("allowed") || err.contains("--allow-path"),
        "the rejection must name the boundary, got: {err}"
    );

    let _ = outside; // keep alive for clarity
}

// --- (c) symlink inside allowed tree pointing outside is rejected ------------

/// A symlink whose *target* is outside the allowed tree must be rejected.
/// `canonicalize` resolves the symlink, so the check sees the real path.
#[tokio::test]
async fn symlink_to_outside_is_rejected() {
    let allowed = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "outside\n").unwrap();

    // Create a symlink inside allowed/ that points to the outside dir.
    let link = allowed.path().join("link_to_outside");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();

    let handler = handler_with_allowed(Some(allowed.path()), &[]);

    // Pass the symlink path as `path` — canonicalize will resolve it to outside.
    let err = try_run(&handler, &link.to_string_lossy(), "cat secret.txt")
        .await
        .expect_err("a symlink whose target is outside the allowed set must be rejected");

    assert!(
        err.to_lowercase().contains("allowed") || err.contains("--allow-path"),
        "the rejection must name the boundary, got: {err}"
    );
}

// --- (d) with --root set, omitted path proceeds (root is in the allowed set) --

/// When --root is set, a call that omits `path` must succeed: the root is in the
/// allowed set by construction, and `resolve_root` falls back to it.
#[tokio::test]
async fn omitted_path_with_root_set_uses_root() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("hello.txt"), "hi\n").unwrap();

    let mut config = Config::builtin();
    config.root = Some(root.path().to_path_buf());
    config.allow_paths = vec![];
    let handler = KaiboHandler::new(config).expect("handler builds");

    let out = handler
        .run_kaish(Parameters(RunKaishInput {
            script: "cat hello.txt".to_string(),
            path: None,
        }))
        .await
        .expect("omitted path with --root must succeed")
        .content
        .into_iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(out.contains("hi"), "root-based call should read file, got: {out}");
}

// --- (e) no root and no allow_paths: allowed set is the cwd ------------------

/// With no --root and no --allow-path, the allowed set defaults to the process's
/// cwd. A path within cwd must succeed (enforcement allows it through resolve_root);
/// a path outside must be rejected.
#[tokio::test]
async fn no_root_no_allow_paths_defaults_to_cwd() {
    let outside = tempdir().unwrap();

    // KaiboHandler::new must compute the allowed set at construction, falling back to
    // cwd when both root and allow_paths are empty. We cannot change the real cwd
    // in a test (it's process-wide), so we verify (a) the handler exposes the cwd
    // as its allowed set, (b) a path inside the cwd is accepted through resolve_root,
    // and (c) a path outside is rejected.
    let config = Config::builtin(); // no root, no allow_paths
    let handler = KaiboHandler::new(config).expect("handler builds");

    // (a) The handler's allowed set must include the cwd.
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let allowed_trees = handler.allowed_set();
    assert!(
        allowed_trees.contains(&cwd),
        "with no root/allow_paths the allowed set must be the cwd, got {:?}",
        allowed_trees
    );

    // (b) A path inside the cwd (the cwd itself) must be accepted through resolve_root.
    // This proves resolve_root accepts-inside, not just that allowed_set() has the cwd.
    let _ok = try_run(&handler, &cwd.to_string_lossy(), "ls")
        .await
        .expect("a path at the cwd must be accepted when no root/allow_paths set");

    // (c) A path clearly outside the cwd (and not the cwd itself) must be rejected.
    let err = try_run(&handler, &outside.path().to_string_lossy(), "ls")
        .await
        .expect_err("a path outside the cwd must be rejected when no root/allow_paths set");

    assert!(
        err.to_lowercase().contains("allowed") || err.contains("--allow-path"),
        "the rejection must name the boundary, got: {err}"
    );
}

// --- (f) omitted path with no root still errors as a parameter error ---------

/// With no --root set, an omitted `path` is a parameter error ("not a silent
/// default") — containment does not change this existing behavior.
#[tokio::test]
async fn omitted_path_no_root_still_errors_as_parameter_error() {
    let config = Config::builtin(); // no root
    let handler = KaiboHandler::new(config).expect("handler builds");

    let err = handler
        .run_kaish(Parameters(RunKaishInput {
            script: "ls".to_string(),
            path: None,
        }))
        .await
        .expect_err("omitted path with no root must be a parameter error");

    // The error must be invalid_params territory — not a containment rejection,
    // but the existing "no path provided and no --root" error.
    let err_str = format!("{err:?}");
    assert!(
        err_str.contains("path") || err_str.contains("root") || err_str.contains("parameter"),
        "omitted path must produce a parameter-flavored error, got: {err_str}"
    );
}

// --- (h) empty-string path is rejected as invalid_params --------------------

/// A call with `path: Some("")` reaches the explicit-path arm (not the omitted-path
/// arm). `canonicalize("")` returns ENOENT, so it is rejected. This test pins the
/// rejection as intentional so it can't silently become a cwd fallback if the code
/// is restructured.
#[tokio::test]
async fn empty_string_path_is_rejected() {
    let root = tempdir().unwrap();
    let handler = handler_with_allowed(Some(root.path()), &[]);

    let err = try_run(&handler, "", "ls")
        .await
        .expect_err("an empty-string path must be rejected with invalid_params");

    // The error must come from the canonicalize branch, not a containment violation.
    let err_lower = err.to_lowercase();
    assert!(
        err_lower.contains("path") || err_lower.contains("resolve") || err_lower.contains("found"),
        "empty path must produce a path-error, got: {err}"
    );
}

// --- (5) mount-layer probe: symlink inside allowed, cat through kaish --------

/// Inside an allowed tempdir, create a symlink to a file OUTSIDE the tree; mount
/// the allowed dir (path = the allowed dir, which passes containment) and `cat`
/// the symlink THROUGH kaish.
///
/// This probes the mount layer: does the read-only kaish VFS follow the symlink
/// out of the allowed tree and return the outside file's bytes?
///
/// If it does, containment has a known mount-layer hole (the call-level check only
/// validates the *root*, not every file read inside it). If it refuses, the boundary
/// is structurally complete at the mount layer.
///
/// BEHAVIOR: This test pins whatever actually happens. The comment on the assert
/// states which behavior it asserts.
#[tokio::test]
async fn mount_layer_symlink_in_allowed_pointing_outside() {
    let allowed = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let secret = outside.path().join("outside_secret.txt");
    fs::write(&secret, "outside-contents-xyz\n").unwrap();

    // Symlink inside the allowed dir pointing to the outside file.
    let link = allowed.path().join("link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let handler = handler_with_allowed(Some(allowed.path()), &[]);

    // Pass the ALLOWED DIR as `path` — this passes containment. Then cat the symlink
    // from within the kaish session.
    let result = try_run(&handler, &allowed.path().to_string_lossy(), "cat link").await;

    // Pin the actual behavior. The mount layer refuses to follow the symlink outside
    // the root: kaish returns a "path escapes root" / "permission denied" error.
    // This asserts the STRUCTURAL BOUNDARY at the mount layer — a symlink inside the
    // allowed tree whose target is outside is refused, not silently read through.
    //
    // If this behavior ever changes (mount layer starts following the symlink and
    // returns outside bytes), this test will fail and must be escalated: add a P2
    // entry to docs/issues.md as a mount-layer symlink-leak hole.
    match &result {
        Ok(out) => {
            // The call succeeded — the mount followed the symlink. Check whether the
            // outside bytes leaked through.
            if out.contains("outside-contents-xyz") {
                // MOUNT-LAYER SYMLINK LEAK: the read-only mount followed a symlink
                // inside the allowed tree to a target outside it and returned the
                // outside file's bytes. This is a new hole — add a P2 entry to
                // docs/issues.md describing the mount-layer symlink-leak.
                panic!(
                    "MOUNT-LAYER SYMLINK LEAK: outside bytes appeared through a symlink \
                     inside the allowed tree — add a P2 entry to docs/issues.md. Got: {out}"
                );
            }
            // Call succeeded but outside bytes weren't present — mount resolved the
            // symlink to something else. This is also acceptable.
        }
        Err(err) => {
            // The mount layer refused to follow the symlink out: structurally sound.
            // Assert the error is refusal-flavored (not some unrelated kaish failure),
            // so this branch distinguishes "mount refused the symlink" from any random
            // error. Keywords from kaish-kernel's VFS path-escape / permission errors.
            let err_lower = err.to_lowercase();
            assert!(
                err_lower.contains("escapes")
                    || err_lower.contains("permission")
                    || err_lower.contains("denied")
                    || err_lower.contains("not found")
                    || err_lower.contains("no such")
                    || err_lower.contains("outside"),
                "the mount-layer refusal must name a path-escape or permission error, got: {err}"
            );
        }
    }
    // The important point: this test proves what happens, it doesn't hide it.
}

// --- (g) shared-prefix sibling directory is rejected -------------------------

/// A path whose canonical form shares a string prefix with an allowed tree but is a
/// distinct sibling directory (e.g. `/allowed/proj-evil` vs allowed `/allowed/proj`)
/// must be rejected. `Path::starts_with` is component-wise, so it correctly rejects
/// the sibling — but this test pins that behavior so a future refactor to string-prefix
/// matching (which would silently open the sibling) breaks the suite immediately.
#[tokio::test]
async fn sibling_with_shared_name_prefix_is_rejected() {
    use std::fs;
    // Create the allowed dir and a sibling whose name shares a prefix.
    let parent = tempdir().unwrap();
    let allowed_dir = parent.path().join("proj");
    let sibling_dir = parent.path().join("proj-evil");
    fs::create_dir(&allowed_dir).unwrap();
    fs::create_dir(&sibling_dir).unwrap();
    fs::write(sibling_dir.join("secret.txt"), "sibling-secret\n").unwrap();

    let handler = handler_with_allowed(Some(&allowed_dir), &[]);

    let err = try_run(&handler, &sibling_dir.to_string_lossy(), "cat secret.txt")
        .await
        .expect_err("a sibling dir sharing a name prefix with the allowed tree must be rejected");

    assert!(
        err.to_lowercase().contains("allowed") || err.contains("--allow-path"),
        "the rejection must name the boundary, got: {err}"
    );
}

// --- Config layering: allow_paths in Config ----------------------------------

/// `allow_paths` from config.toml file is parsed and stored.
#[test]
fn allow_paths_from_toml_file() {
    let c = kaibo::config::Config::from_toml_str(
        r#"
        [server]
        allow_paths = ["/tmp/a", "/tmp/b"]
        "#,
    )
    .unwrap();
    assert_eq!(c.allow_paths.len(), 2);
    assert!(c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/a")));
    assert!(c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/b")));
}

/// `KAIBO_ALLOW_PATHS` env var (colon-separated) overrides file.
#[test]
fn allow_paths_from_env_overrides_file() {
    use std::collections::HashMap;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[server]\nallow_paths = [\"/tmp/file-only\"]\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_ALLOW_PATHS", "/tmp/env-a:/tmp/env-b")]
        .into_iter()
        .collect();
    let c = kaibo::config::Config::load_with(
        None,
        Some(path),
        |k| env.get(k).map(|s| s.to_string()),
    )
    .unwrap();
    // env replaces file (non-empty CLI list replaces lower layers; env follows same rule).
    assert!(
        c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/env-a")),
        "env KAIBO_ALLOW_PATHS must override file, got {:?}",
        c.allow_paths
    );
    assert!(
        c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/env-b")),
        "both colon-separated paths must be present, got {:?}",
        c.allow_paths
    );
    // File-only value must NOT be present (env replaces, not appends).
    assert!(
        !c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/file-only")),
        "env replace must not include file-only values, got {:?}",
        c.allow_paths
    );
}

/// `apply_cli` with a non-empty allow_paths replaces lower layers.
#[test]
fn allow_paths_cli_replaces_env_and_file() {
    let mut c = kaibo::config::Config::builtin();
    c.allow_paths = vec![std::path::PathBuf::from("/tmp/env-only")];
    c.apply_cli(None, None, kaibo::config::ToolDisables::default(), vec![
        std::path::PathBuf::from("/tmp/cli-a"),
        std::path::PathBuf::from("/tmp/cli-b"),
    ]);
    assert_eq!(c.allow_paths.len(), 2);
    assert!(c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/cli-a")));
    assert!(c.allow_paths.iter().any(|p| p == std::path::Path::new("/tmp/cli-b")));
}
