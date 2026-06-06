//! Credential resolution — pure, no real env or `$HOME` touched.

use std::fs;
use std::str::FromStr;

use kaibo::credentials::{
    load, resolve, resolve_base_url, resolve_openai_key, Provider, DEFAULT_OPENAI_BASE_URL,
    PLACEHOLDER_OPENAI_KEY,
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
    for s in ["openai", "OpenAI", "local", "lemonade", "  GEMMA ", "gemma4"] {
        assert_eq!(
            Provider::from_str(s).unwrap(),
            Provider::Openai,
            "{s:?} should parse as Openai"
        );
    }
}

#[test]
fn only_openai_tolerates_a_missing_key() {
    assert!(Provider::Openai.key_optional());
    assert!(!Provider::Anthropic.key_optional());
    assert!(!Provider::DeepSeek.key_optional());
    assert!(!Provider::Gemini.key_optional());
}

#[test]
fn load_refuses_the_key_optional_provider_loudly() {
    // OpenAI's key may legitimately be absent; asking load() for it is a
    // programming error we surface rather than letting it masquerade as a
    // missing-credential failure. Callers must use openai_key() instead.
    let err = load(Provider::Openai).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("openai_key"),
        "got: {err}"
    );
}

#[test]
fn openai_key_uses_a_configured_key_when_present() {
    let dir = tempdir().unwrap();
    let file = dir.path().join(".openai-key");
    fs::write(&file, "sk-real\n").unwrap();

    // Env wins over file, file used when env absent — same precedence as resolve().
    assert_eq!(resolve_openai_key(Some("sk-env"), &file), "sk-env");
    assert_eq!(resolve_openai_key(None, &file), "sk-real");
}

#[test]
fn openai_key_falls_back_to_placeholder_when_unset() {
    // The keyless local default: no env, no file -> a placeholder, NOT an error.
    // (resolve() would error here; resolve_openai_key() must not.)
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    assert_eq!(resolve_openai_key(None, &missing), PLACEHOLDER_OPENAI_KEY);
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
        Provider::Anthropic.key_file(home),
        home.join(".anthropic-key.txt")
    );
    assert_eq!(Provider::DeepSeek.key_file(home), home.join(".deepseek-key"));
    assert_eq!(Provider::Gemini.key_file(home), home.join(".gemini-api-key"));
    assert_eq!(Provider::Openai.key_file(home), home.join(".openai-key"));
}
