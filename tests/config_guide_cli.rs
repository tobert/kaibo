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
