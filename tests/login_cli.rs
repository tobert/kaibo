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

/// Sign-ins do not depend on session persistence: they live in their own credentials
/// db, whose directory kaibo creates private.
#[test]
fn sign_in_works_with_persistence_off_and_keeps_its_dir_private() {
    let home = tempfile::tempdir().unwrap();
    let out = kaibo(home.path(), &["--no-persistence", "login", "--status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not signed in"),
        "{}",
        text(&out)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = home.path().join("state/kaibo/credentials");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
}

/// A credentials directory left open to other users is made private, and kaibo says so.
#[cfg(unix)]
#[test]
fn an_open_credentials_dir_is_fixed_and_logged() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("state/kaibo/credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = kaibo(home.path(), &["login", "--status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mode 700"),
        "the change is logged: {}",
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
