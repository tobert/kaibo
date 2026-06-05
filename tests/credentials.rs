//! Credential resolution — pure, no real env or `$HOME` touched.

use std::fs;
use std::str::FromStr;

use kaibo::credentials::{
    load, resolve, resolve_base_url, Provider, DEFAULT_LEMONADE_BASE_URL,
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

// --- Lemonade: a local, keyless provider addressed by base URL, not an API key.

#[test]
fn lemonade_parses_from_friendly_aliases() {
    // The default provider clients call it gemma in conversation; accept the
    // model-name aliases as well as the canonical "lemonade".
    for s in ["lemonade", "Lemonade", "  GEMMA ", "gemma4", "local"] {
        assert_eq!(
            Provider::from_str(s).unwrap(),
            Provider::Lemonade,
            "{s:?} should parse as Lemonade"
        );
    }
}

#[test]
fn lemonade_is_local_the_keyed_providers_are_not() {
    assert!(Provider::Lemonade.is_local());
    assert!(!Provider::Anthropic.is_local());
    assert!(!Provider::DeepSeek.is_local());
    assert!(!Provider::Gemini.is_local());
}

#[test]
fn load_refuses_a_local_provider_loudly() {
    // A local provider has no API key; asking to load one is a programming
    // error we want surfaced, not a silent empty key sent to a server.
    let err = load(Provider::Lemonade).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("local"),
        "got: {err}"
    );
}

#[test]
fn lemonade_base_url_defaults_when_env_absent_or_blank() {
    assert_eq!(resolve_base_url(None), DEFAULT_LEMONADE_BASE_URL);
    assert_eq!(resolve_base_url(Some("   ")), DEFAULT_LEMONADE_BASE_URL);
}

#[test]
fn lemonade_base_url_env_wins_and_is_trimmed() {
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
}
