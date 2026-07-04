//! Credential resolution — pure, no real env or `$HOME` touched.

use std::fs;
use std::str::FromStr;

use kaibo::credentials::{resolve, resolve_base_url, ProviderKind, DEFAULT_OPENAI_BASE_URL};
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
    assert_eq!(ProviderKind::DeepSeek.key_file(home), home.join(".deepseek-key"));
    assert_eq!(ProviderKind::Gemini.key_file(home), home.join(".gemini-api-key"));
    assert_eq!(
        ProviderKind::OpenRouter.key_file(home),
        home.join(".openrouter-key")
    );
    assert_eq!(ProviderKind::Openai.key_file(home), home.join(".openai-key"));
}
