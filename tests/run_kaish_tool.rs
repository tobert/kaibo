//! The sandbox boundary through the NEW front door: the caller-facing `run_kaish`
//! #[tool]. Same invariant as `tests/sandbox.rs`, but exercised the way a real MCP
//! caller hits it — through `KaiboHandler`, not rig's internal `RunKaish`.
//!
//! Teeth (manual, as in `tests/sandbox.rs`): temporarily set
//! `sandbox::DENYLIST = &[]` and the `touch`/`git` assertions below fail — those
//! builtins reach real state directly and only the denylist catches them. The
//! `rm`/redirect cases get their teeth from the read-only mount independently.

use std::fs;
use std::path::Path;

use kaibo::credentials::Provider;
use kaibo::server::{KaiboHandler, RunKaishInput, ToolGating};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::tempdir;

/// Drive the caller-facing `run_kaish` tool and flatten its content to text.
async fn run_tool(handler: &KaiboHandler, path: &Path, script: &str) -> String {
    let out = handler
        .run_kaish(Parameters(RunKaishInput {
            script: script.to_string(),
            path: Some(path.to_string_lossy().into_owned()),
        }))
        .await
        .expect("run_kaish tool call should not be an MCP-level error");
    out.content
        .into_iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn handler_for(_root: &Path) -> KaiboHandler {
    // No default root: the test passes `path` explicitly each call.
    KaiboHandler::new(None, Provider::Anthropic, ToolGating::default())
}

#[tokio::test]
async fn reads_work_through_the_front_door() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "kai the crab\n").unwrap();

    let handler = handler_for(dir.path());
    let text = run_tool(&handler, dir.path(), "cat hello.txt").await;

    assert!(text.contains("exit: 0"), "cat should succeed, got: {text}");
    assert!(text.contains("kai the crab"), "should echo file contents, got: {text}");
}

#[tokio::test]
async fn rm_is_refused_and_the_file_survives() {
    let dir = tempdir().unwrap();
    let victim = dir.path().join("important.txt");
    fs::write(&victim, "keep me\n").unwrap();

    let handler = handler_for(dir.path());
    let text = run_tool(&handler, dir.path(), "rm important.txt").await;

    assert!(!text.contains("exit: 0"), "rm must not succeed, got: {text}");
    assert!(victim.exists(), "the real file must survive a refused rm");
}

#[tokio::test]
async fn redirect_write_does_not_touch_disk() {
    let dir = tempdir().unwrap();

    let handler = handler_for(dir.path());
    let _ = run_tool(&handler, dir.path(), "echo pwned > newfile.txt").await;

    assert!(
        !dir.path().join("newfile.txt").exists(),
        "a redirect must not create a real file on disk"
    );
}

#[tokio::test]
async fn touch_on_existing_file_is_denylist_blocked() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("real.txt");
    fs::write(&target, "x\n").unwrap();
    let before = fs::metadata(&target).unwrap().modified().unwrap();

    let handler = handler_for(dir.path());
    let text = run_tool(&handler, dir.path(), "touch real.txt").await;

    assert!(!text.contains("exit: 0"), "touch must be refused, got: {text}");
    assert!(
        text.contains("read-only sandbox"),
        "the denylist (not the mount) should catch the std::fs mtime path, got: {text}"
    );
    let after = fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(before, after, "the real file's mtime must not change");
}

#[tokio::test]
async fn git_is_blocked_and_inits_no_repo() {
    let dir = tempdir().unwrap();

    let handler = handler_for(dir.path());
    let text = run_tool(&handler, dir.path(), "git init").await;

    assert!(!text.contains("exit: 0"), "git must be refused, got: {text}");
    assert!(
        !dir.path().join(".git").exists(),
        "git must not create a real .git directory"
    );
}

#[tokio::test]
async fn external_commands_are_refused() {
    let dir = tempdir().unwrap();

    let handler = handler_for(dir.path());
    let text = run_tool(&handler, dir.path(), "/bin/sh -c 'echo escaped'").await;

    assert!(!text.contains("escaped"), "external command must not run, got: {text}");
}
