//! Provider credentials, from key-files with an env-var override.
//!
//! Long-term kaibo will take credentials from both files and env. For now the
//! source of truth is a per-provider dotfile in `$HOME`; if the matching env var
//! is set it wins (handy for CI / one-off overrides).
//!
//! - Anthropic: `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt`
//! - DeepSeek:  `DEEPSEEK_API_KEY`  / `~/.deepseek-key`
//! - Gemini:    `GEMINI_API_KEY`    / `~/.gemini-api-key`
//!
//! [`Provider::Openai`] is the general case: any endpoint speaking the OpenAI
//! wire protocol, addressed by [`openai_base_url`] (`OPENAI_BASE_URL`, default a
//! local keyless server) rather than tied to a hosted service. Its key is
//! *optional* — `OPENAI_API_KEY` / `~/.openai-key` when talking to a keyed
//! endpoint, or a placeholder for the keyless local default (see [`openai_key`]).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Default OpenAI-compatible endpoint: a local, keyless server (e.g. an OpenAI-
/// compatible host such as AMD's Lemonade serving Gemma). The `/api/v1` suffix
/// matters — rig posts to `{base_url}/chat/completions`. Override with the
/// `OPENAI_BASE_URL` env var (see [`openai_base_url`]).
pub const DEFAULT_OPENAI_BASE_URL: &str = "http://localhost:13305/api/v1";

/// A model provider. The keyed providers (Anthropic/DeepSeek/Gemini) each speak
/// their own wire protocol and require an API key. [`Provider::Openai`] is the
/// generic OpenAI-compatible endpoint: any base URL speaking that protocol, with
/// an *optional* key — keyless by default, since the default endpoint is local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    DeepSeek,
    Gemini,
    /// Any OpenAI-compatible endpoint, addressed by base URL; key optional.
    /// Defaults to a local keyless server (Gemma via an OpenAI-compatible host).
    Openai,
}

impl Provider {
    /// Whether a missing credential is tolerated rather than a hard error. Only
    /// the OpenAI-compatible provider is: its default endpoint is a local keyless
    /// server, so an absent key falls back to a placeholder bearer token (see
    /// [`openai_key`]). The keyed providers must fail loudly on a missing key.
    pub fn key_optional(self) -> bool {
        matches!(self, Provider::Openai)
    }

    /// The environment variable that overrides the key-file.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::Openai => "OPENAI_API_KEY",
        }
    }

    /// The key-file's name within `$HOME`. For the OpenAI provider the key is
    /// optional (see [`Provider::key_optional`]); the file is consulted only if
    /// present.
    pub fn key_file_name(self) -> &'static str {
        match self {
            Provider::Anthropic => ".anthropic-key.txt",
            Provider::DeepSeek => ".deepseek-key",
            Provider::Gemini => ".gemini-api-key",
            Provider::Openai => ".openai-key",
        }
    }

    /// The key-file path under the given home directory.
    pub fn key_file(self, home: &Path) -> PathBuf {
        home.join(self.key_file_name())
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Provider::Anthropic),
            "deepseek" => Ok(Provider::DeepSeek),
            "gemini" | "google" => Ok(Provider::Gemini),
            // The OpenAI-compatible endpoint. Also accept the names people reach
            // for when it points at the local keyless default (Gemma via Lemonade).
            "openai" | "local" | "lemonade" | "gemma" | "gemma4" => Ok(Provider::Openai),
            other => Err(anyhow!(
                "unknown provider {other:?} (expected anthropic, deepseek, gemini, or openai)"
            )),
        }
    }
}

/// Resolve the OpenAI base URL from an explicit env value, defaulting when unset
/// or blank. Pure so it can be tested without touching the environment.
pub fn resolve_base_url(env_value: Option<&str>) -> String {
    match env_value {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_OPENAI_BASE_URL.to_string(),
    }
}

/// The OpenAI-compatible endpoint, from `OPENAI_BASE_URL` or the default.
pub fn openai_base_url() -> String {
    resolve_base_url(std::env::var("OPENAI_BASE_URL").ok().as_deref())
}

/// Placeholder bearer token for the keyless local default. The OpenAI client
/// builder rejects an empty key, but a local server ignores the value entirely.
pub const PLACEHOLDER_OPENAI_KEY: &str = "no-auth";

/// Resolve the OpenAI bearer token: the configured key when present (env over
/// file), else the placeholder. Pure for testing. Unlike [`resolve`], a missing
/// credential is NOT an error — the default endpoint is a local keyless server,
/// so we fall back rather than refuse.
pub fn resolve_openai_key(env_value: Option<&str>, key_file: &Path) -> String {
    resolve(env_value, key_file).unwrap_or_else(|_| PLACEHOLDER_OPENAI_KEY.to_string())
}

/// The OpenAI bearer token from `OPENAI_API_KEY` / `~/.openai-key`, or the
/// placeholder when neither is set (the keyless local default).
pub fn openai_key() -> String {
    let key_file = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| Provider::Openai.key_file(&home))
        .unwrap_or_else(|| PathBuf::from("/nonexistent/.openai-key"));
    let env_value = std::env::var(Provider::Openai.env_var()).ok();
    resolve_openai_key(env_value.as_deref(), &key_file)
}

/// Resolve a key from an explicit env value and a key-file, env winning.
///
/// Pure so it can be tested without touching the real environment or `$HOME`.
/// A whitespace-only env value or file is treated as absent — an empty key is a
/// configuration mistake we'd rather surface than silently send to the API.
pub fn resolve(env_value: Option<&str>, key_file: &Path) -> Result<String> {
    if let Some(v) = env_value {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }

    match std::fs::read_to_string(key_file) {
        Ok(contents) => {
            let key = contents.trim();
            if key.is_empty() {
                return Err(anyhow!("key-file {} is empty", key_file.display()));
            }
            Ok(key.to_string())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "no credential for this provider: env var unset and key-file {} not found",
            key_file.display()
        )),
        Err(e) => Err(e).with_context(|| format!("reading key-file {}", key_file.display())),
    }
}

/// Load `provider`'s key from the real environment and `$HOME`.
///
/// The OpenAI provider's key is optional; calling this for it is a programming
/// error — its key may legitimately be absent, so route through [`openai_key`],
/// which falls back to a placeholder rather than refusing.
pub fn load(provider: Provider) -> Result<String> {
    if provider.key_optional() {
        return Err(anyhow!(
            "{provider:?} tolerates a missing key — use openai_key(), not load()"
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME is not set; cannot locate key-files"))?;
    let env_value = std::env::var(provider.env_var()).ok();
    resolve(env_value.as_deref(), &provider.key_file(&home))
        .with_context(|| format!("loading {:?} credentials", provider))
}
