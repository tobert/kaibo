//! Recovery guidance stays available before config, telemetry, or credentials load.
use std::process::Command;

#[test]
fn guide_prints_without_loading_broken_config() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("broken.toml");
    std::fs::write(&config, "[broken config").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path())
        .env("XDG_DATA_HOME", home.path())
        .env("KAIBO_CONFIG", &config)
        .arg("config-guide")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        include_str!("../docs/config.md")
    );
    assert!(result.stderr.is_empty());
}

/// `kaibo host-guide` lists the hosts, prints one guide, and refuses an unknown host with
/// exit 2 and the list, all without reading config.
#[test]
fn host_guide_lists_prints_and_refuses() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaibo"))
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .args(args)
            .output()
            .unwrap()
    };
    let list = run(&["host-guide"]);
    assert!(list.status.success());
    let list = String::from_utf8(list.stdout).unwrap();
    for name in ["claude-code", "codex", "opencode", "agy", "claude-desktop"] {
        assert!(list.contains(&format!("kaibo host-guide {name}")), "{list}");
    }

    let codex = run(&["host-guide", "codex"]);
    assert!(codex.status.success());
    assert_eq!(
        String::from_utf8(codex.stdout).unwrap(),
        include_str!("../docs/hosts/codex.md")
    );

    let unknown = run(&["host-guide", "gemini-cli"]);
    assert_eq!(unknown.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("claude-code") && stderr.contains("agy"),
        "{stderr}"
    );
}
