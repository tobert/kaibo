//! Credential resolution — pure, no real env or `$HOME` touched.

use std::fs;

use kaibo::credentials::{resolve, Provider};
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
