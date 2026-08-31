//! Credential resolution — pure, no real env or `$HOME` touched.

use std::fs;
use std::str::FromStr;

use kaibo::credentials::{
    resolve, resolve_base_url, ProviderKind, DEFAULT_OPENAI_BASE_URL, PLACEHOLDER_OPENAI_KEY,
};
use tempfile::tempdir;

#[test]
fn env_value_wins_over_file() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("key");
    fs::write(&file, "from-file\n").unwrap();

    let got = resolve(Some("from-env"), &file).unwrap();
    assert_eq!(got, "from-env");
}

#[test]
fn falls_back_to_file_when_env_absent() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("key");
    fs::write(&file, "  from-file\n\n").unwrap(); // surrounding whitespace trimmed

    let got = resolve(None, &file).unwrap();
    assert_eq!(got, "from-file");
}

#[test]
fn whitespace_only_env_is_treated_as_absent() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("key");
    fs::write(&file, "real-key\n").unwrap();

    let got = resolve(Some("   "), &file).unwrap();
    assert_eq!(got, "real-key");
}

#[test]
fn missing_file_and_no_env_is_an_error() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("does-not-exist");

    let err = resolve(None, &file).unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

#[test]
fn empty_file_is_an_error_not_an_empty_key() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("key");
    fs::write(&file, "\n  \n").unwrap();

    let err = resolve(None, &file).unwrap_err();
    assert!(err.to_string().contains("empty"), "got: {err}");
}

// --- OpenAI: any OpenAI-compatible endpoint, addressed by base URL; key optional.

#[test]
fn openai_parses_from_friendly_aliases() {
    // Canonical "openai", plus the names people reach for when it points at the
    // local keyless default (Gemma served by Lemonade).
    for s in [
        "openai", "OpenAI", "local", "lemonade", "  GEMMA ", "gemma4",
    ] {
        assert_eq!(
            ProviderKind::from_str(s).unwrap(),
            ProviderKind::Openai,
            "{s:?} should parse as Openai"
        );
    }
}

#[test]
fn only_openai_tolerates_a_missing_key() {
    assert!(ProviderKind::Openai.key_optional());
    assert!(!ProviderKind::Anthropic.key_optional());
    assert!(!ProviderKind::DeepSeek.key_optional());
    assert!(!ProviderKind::Gemini.key_optional());
    // OpenRouter is a *keyed* gateway — a missing key is a hard error, not tolerated.
    assert!(!ProviderKind::OpenRouter.key_optional());
}

#[test]
fn placeholder_key_is_empty_only_for_gemini() {
    // Gemini's `?key=` query auth wants an EMPTY keyless stand-in (rig emits a bare
    // `?key=` an ambient-auth gateway accepts); every header-auth kind keeps the
    // non-empty bearer placeholder a keyless server ignores. Exhaustive so a new
    // provider forces a keyless-transport decision rather than defaulting silently.
    assert_eq!(ProviderKind::Gemini.placeholder_key(), "");
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::OpenRouter,
        ProviderKind::Openai,
    ] {
        assert_eq!(
            kind.placeholder_key(),
            PLACEHOLDER_OPENAI_KEY,
            "{kind:?} authenticates with a header and needs a non-empty keyless stand-in"
        );
        assert!(!kind.placeholder_key().is_empty());
    }
}

// --- OpenRouter: a keyed gateway (one key, fixed endpoint) fronting every model.

#[test]
fn openrouter_parses_and_carries_its_key_source() {
    assert_eq!(
        ProviderKind::from_str("openrouter").unwrap(),
        ProviderKind::OpenRouter
    );
    assert_eq!(
        ProviderKind::from_str("  OpenRouter ").unwrap(),
        ProviderKind::OpenRouter
    );
    assert_eq!(ProviderKind::OpenRouter.canonical_name(), "openrouter");
    assert_eq!(ProviderKind::OpenRouter.builtin_name(), "openrouter");
    assert_eq!(ProviderKind::OpenRouter.env_var(), "OPENROUTER_API_KEY");
    assert_eq!(
        ProviderKind::OpenRouter.key_file(std::path::Path::new("/home/amy")),
        std::path::Path::new("/home/amy/.openrouter-key")
    );
}

#[test]
fn openrouter_key_resolves_from_env_then_file() {
    let dir = tempdir().unwrap();
    let file = dir.path().join(ProviderKind::OpenRouter.key_file_name());
    fs::write(&file, "sk-or-from-file\n").unwrap();
    // Env wins...
    assert_eq!(resolve(Some("sk-or-env"), &file).unwrap(), "sk-or-env");
    // ...and the file is the fallback.
    assert_eq!(resolve(None, &file).unwrap(), "sk-or-from-file");
}

#[test]
fn unknown_provider_error_lists_openrouter() {
    let err = ProviderKind::from_str("nope").unwrap_err();
    assert!(
        err.to_string().contains("openrouter"),
        "the error should list openrouter among the expected kinds: {err}"
    );
}

#[test]
fn openai_base_url_defaults_when_env_absent_or_blank() {
    assert_eq!(resolve_base_url(None), DEFAULT_OPENAI_BASE_URL);
    assert_eq!(resolve_base_url(Some("   ")), DEFAULT_OPENAI_BASE_URL);
}

#[test]
fn openai_base_url_env_wins_and_is_trimmed() {
    assert_eq!(
        resolve_base_url(Some("  http://box:9000/api/v1\n")),
        "http://box:9000/api/v1"
    );
}

#[test]
fn provider_paths_match_amys_dotfiles() {
    let home = std::path::Path::new("/home/amy");
    assert_eq!(
        ProviderKind::Anthropic.key_file(home),
        home.join(".anthropic-key.txt")
    );
    assert_eq!(
        ProviderKind::DeepSeek.key_file(home),
        home.join(".deepseek-key")
    );
    assert_eq!(
        ProviderKind::Gemini.key_file(home),
        home.join(".gemini-api-key")
    );
    assert_eq!(
        ProviderKind::OpenRouter.key_file(home),
        home.join(".openrouter-key")
    );
    assert_eq!(
        ProviderKind::Openai.key_file(home),
        home.join(".openai-key")
    );
}

// --- api_key_cmd: the operator-declared command source -----------------------
//
// The exec core (`resolve_key_from_cmd`) is exercised with `#!/bin/sh` stubs,
// invoked by ABSOLUTE PATH (no PATH dependence, no child-env rebuild) in temp
// dirs. Unix-only: a stub needs a shell at the executable.
//
// The blank-env discipline: the child inherits the test process env, but nothing
// below depends on what that env contains — a stub reads only what it is given
// (its own argv, `extra_env`). `extra_env` is the seam a unit test uses instead
// of mutating the process environment.

use kaibo::credentials::resolve_key_from_cmd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static STUB_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn stub_script(dir: &Path, body: &str, exit: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let seq = STUB_SEQ.fetch_add(1, Ordering::Relaxed);
    let p = dir.join(format!("stub-{seq}"));
    fs::write(&p, format!("#!/bin/sh\n{body}\nexit {exit}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn key_command_stdout_trimmed_is_the_key() {
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), "printf 'sk-test\\n\\n'", 0);
    let args = vec![p.to_string_lossy().into_owned()];
    let key = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap();
    assert_eq!(
        key, "sk-test",
        "stdout is trimmed like a key file's contents"
    );
}

#[cfg(unix)]
#[test]
fn key_command_inherits_env_and_receives_argv() {
    // `extra_env` is the test seam for what a real child sees inherited (HOME,
    // OP_SERVICE_ACCOUNT_TOKEN); later argv elements pass through verbatim.
    // Note: this proves the INJECTION seam, not raw process-env inheritance —
    // `Command::new` inherits by default and nothing on this path calls
    // `env_clear`, but asserting the ambient env would need a process-env
    // mutation, which the blank-env discipline forbids (see stability.rs).
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), "echo \"$KAIBO_STUB_VAR $1\"", 0);
    let args = vec![p.to_string_lossy().into_owned(), "op://Vault/Item".into()];
    let key = resolve_key_from_cmd(
        &args,
        Duration::from_secs(5),
        &[("KAIBO_STUB_VAR", "inherited")],
    )
    .unwrap();
    assert_eq!(key, "inherited op://Vault/Item");
}

#[cfg(unix)]
#[test]
fn key_command_empty_stdout_is_loud() {
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), ":", 0); // prints nothing, exits 0
    let args = vec![p.to_string_lossy().into_owned()];
    let err = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap_err();
    assert!(
        err.to_string().contains("printed nothing"),
        "an empty stdout is a broken command, not a keyless backend: {err}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_non_utf8_stdout_is_loud() {
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), "printf '\\377'", 0); // invalid UTF-8
    let args = vec![p.to_string_lossy().into_owned()];
    let err = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap_err();
    assert!(
        err.to_string().contains("non-UTF-8"),
        "binary output must be surfaced, not mangled into a key: {err}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_oversized_stdout_is_refused_not_trimmed() {
    let dir = tempdir().unwrap();
    // 70 KiB of output: past the 64 KiB cap, and past the pipe buffer, so this
    // also proves the reader drains a verbose child instead of wedging it.
    let p = stub_script(dir.path(), "head -c 70000 /dev/zero", 0);
    let args = vec![p.to_string_lossy().into_owned()];
    let err = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap_err();
    assert!(
        err.to_string().contains("more than 65536 bytes"),
        "refuse, not trim: {err}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_nonzero_exit_is_loud_and_never_leaks_stdout_or_stderr() {
    let dir = tempdir().unwrap();
    // The child says something secret on stdout, and something that COULD be a
    // secret on stderr too (a wrapper traced with `set -x` prints exactly this
    // shape): the error names the exit status and a stderr byte count, and NEVER
    // quotes either stream's content — stdout because a broken command may print
    // anything there, stderr because it can carry the secret as easily as stdout.
    let p = stub_script(
        dir.path(),
        "printf 'TOP-SECRET-KEY-MATERIAL\\n'; echo 'Vault is locked' >&2",
        3,
    );
    let args = vec![p.to_string_lossy().into_owned()];
    let err = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("exited"), "must name the failure: {msg}");
    assert!(
        msg.contains("suppressed"),
        "must say stderr was suppressed rather than shown: {msg}"
    );
    assert!(
        msg.contains("bytes on stderr"),
        "a bounded, safe summary (a byte count) stands in for stderr's content: {msg}"
    );
    assert!(
        !msg.contains("Vault is locked"),
        "stderr's content must never reach the error — it can carry the secret itself: {msg}"
    );
    assert!(
        !msg.contains("TOP-SECRET-KEY-MATERIAL"),
        "stdout must never appear in an error: {msg}"
    );
}

// Not gated `#[cfg(unix)]` like its neighbors: the empty-argv guard returns before
// `Command` is ever touched, so it needs no shell stub and runs on every platform.
#[test]
fn key_command_empty_argv_is_a_loud_error_not_a_panic() {
    let err = resolve_key_from_cmd(&[], Duration::from_secs(5), &[]).unwrap_err();
    assert!(
        err.to_string().contains("api_key_cmd") && err.to_string().contains("executable"),
        "an empty argv is a config error naming the field and the fix, not a panic: {err}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_missing_binary_is_loud() {
    let args = vec!["/nonexistent-kaibo-test/op".to_string()];
    let err = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap_err();
    assert!(
        err.to_string().contains("spawning key command"),
        "a typo'd binary name fails at resolve, not silently: {err}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_timeout_kills_a_hung_child() {
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), "sleep 60", 0); // would hang forever
    let args = vec![p.to_string_lossy().into_owned()];
    let err = resolve_key_from_cmd(&args, Duration::from_millis(200), &[]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("did not exit within") && msg.contains("killed"),
        "a hung command is killed and named, not left to wedge resolution: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn key_command_stdin_is_nulled() {
    // The child's read sees immediate EOF (stdin is closed, not inherited) AND
    // its fd 0 is the null device — the two checks cover each other's blind
    // spots: on a runner whose own stdin is /dev/null the EOF check alone would
    // pass even if the pin were removed, and a char-device check alone would
    // pass on a TTY-backed runner. The MCP-stream property itself (the child
    // must never read kaibo's protocol stdin) is structurally pinned by
    // `Stdio::null()` in `resolve_key_from_cmd`.
    let dir = tempdir().unwrap();
    let p = stub_script(
        dir.path(),
        "if [ -c /dev/stdin ] && ! read -r x; then echo null-stdin; else \
         echo got-stdin; fi",
        0,
    );
    let args = vec![p.to_string_lossy().into_owned()];
    let key = resolve_key_from_cmd(&args, Duration::from_secs(5), &[]).unwrap();
    assert_eq!(key, "null-stdin", "stdin must be closed, not inherited");
}

/// The concurrency property the async design leans on, codified: key resolution
/// on a multi-thread tokio runtime must not stall a sibling task — kaibo runs
/// `#[tokio::main]` and its whole client-construction chain is sync, so a bounded
/// (≤30s) blocking resolve occupies one worker while others keep serving. A
/// sleeping key command (350 ms) plus a ticker that must fire mid-sleep proves it;
/// this test turns red the day the runtime flavor changes to single-threaded or
/// the resolution path stops being safely blockable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocking_key_resolve_does_not_stall_a_sibling_task() {
    let dir = tempdir().unwrap();
    let p = stub_script(dir.path(), "sleep 1", 0);
    let args = vec![p.to_string_lossy().into_owned()];

    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        let mut ticks = 0u32;
        for _ in 0..12 {
            interval.tick().await;
            ticks += 1;
        }
        ticks
    });
    let key = resolve_key_from_cmd(&args, Duration::from_millis(300), &[]).unwrap_err();
    assert!(
        key.to_string().contains("did not exit within"),
        "the resolve must time out on the sleeping stub: {key}"
    );
    let ticks = ticker.await.expect("ticker task completes");
    assert!(
        ticks >= 2,
        "a sibling task must keep progressing while the key command blocks \
         (got {ticks} ticks in ~300ms)"
    );
}
