//! `kaibo login` / `kaibo logout` as a person runs them: the real binary, a blank
//! environment (temp `HOME` and XDG roots only), and a config naming signed-in
//! backends. No provider is contacted: these paths stop before the browser step.
use std::path::Path;
use std::process::{Command, Output};

const CONFIG: &str = r#"
[backends.plan]
kind = "openai"
auth = "chatgpt"

[backends.keyed]
kind = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
"#;

/// Run kaibo with `home` as HOME and every XDG root, and the project in
/// `home/project` (the state db must live outside the allowed tree).
fn kaibo(home: &Path, args: &[&str]) -> Output {
    let config = home.join("config.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home)
        .env("KAIBO_CONFIG", &config)
        .args(["--root", project.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn status_lists_declared_backends_without_contacting_anyone() {
    let home = tempfile::tempdir().unwrap();
    let out = kaibo(home.path(), &["login", "--status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("plan: not signed in — run `kaibo login plan`"),
        "{}",
        text(&out)
    );
    assert!(
        !stdout.contains("keyed"),
        "a keyed backend has nothing to sign in: {}",
        text(&out)
    );
}

#[test]
fn login_refuses_a_keyed_backend_with_the_fix() {
    let home = tempfile::tempdir().unwrap();
    let out = kaibo(home.path(), &["login", "keyed"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("has no `auth` key"), "{}", text(&out));
    assert!(
        stderr.contains("auth = \\\"chatgpt\\\"") || stderr.contains("auth = \"chatgpt\""),
        "{}",
        text(&out)
    );
}

#[test]
fn login_needs_persistence_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    let out = kaibo(home.path(), &["--no-persistence", "login", "plan"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("persistence is off"),
        "{}",
        text(&out)
    );
}

#[test]
fn logout_of_an_unsigned_backend_is_a_quiet_success() {
    let home = tempfile::tempdir().unwrap();
    let out = kaibo(home.path(), &["logout", "plan"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no stored sign-in"),
        "{}",
        text(&out)
    );
}
