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
//! [`Provider::Lemonade`] is the exception: a *local*, keyless server (an OpenAI-
//! compatible endpoint, e.g. AMD's lemonade serving Gemma) addressed by a base
//! URL, not an API key. It carries no key-file; [`load`] refuses it, and
//! [`lemonade_base_url`] supplies the endpoint instead.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Where the local lemonade server's OpenAI-compatible API lives by default.
/// The `/api/v1` suffix matters — rig posts to `{base_url}/chat/completions`.
/// Override with the `LEMONADE_BASE_URL` env var (see [`lemonade_base_url`]).
pub const DEFAULT_LEMONADE_BASE_URL: &str = "http://localhost:13305/api/v1";

/// A model provider. Keyed providers (Anthropic/DeepSeek/Gemini) authenticate
/// with an API key; [`Provider::Lemonade`] is a local server reached by URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    DeepSeek,
    Gemini,
    /// A local, keyless OpenAI-compatible server (lemonade, serving Gemma).
    Lemonade,
}

impl Provider {
    /// Whether this provider is a local, keyless endpoint addressed by base URL.
    /// Local providers have no key-file and must not be passed to [`load`].
    pub fn is_local(self) -> bool {
        matches!(self, Provider::Lemonade)
    }

    /// The environment variable that overrides the key-file.
    ///
    /// Only meaningful for keyed providers; a local provider has no API key, so
    /// this panics rather than invent one (see [`Provider::is_local`]).
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::Lemonade => {
                unreachable!("Lemonade is local and keyless; gate on is_local() before env_var()")
            }
        }
    }

    /// The key-file's name within `$HOME`. Keyed providers only — see [`env_var`].
    ///
    /// [`env_var`]: Provider::env_var
    pub fn key_file_name(self) -> &'static str {
        match self {
            Provider::Anthropic => ".anthropic-key.txt",
            Provider::DeepSeek => ".deepseek-key",
            Provider::Gemini => ".gemini-api-key",
            Provider::Lemonade => {
                unreachable!("Lemonade is local and keyless; gate on is_local() before key_file()")
            }
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
            // Accept the model names people actually say for the local server.
            "lemonade" | "local" | "gemma" | "gemma4" => Ok(Provider::Lemonade),
            other => Err(anyhow!(
                "unknown provider {other:?} (expected anthropic, deepseek, gemini, or lemonade)"
            )),
        }
    }
}

/// Resolve the lemonade base URL from an explicit env value, defaulting when
/// unset or blank. Pure so it can be tested without touching the environment.
pub fn resolve_base_url(env_value: Option<&str>) -> String {
    match env_value {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_LEMONADE_BASE_URL.to_string(),
    }
}

/// The local lemonade endpoint, from `LEMONADE_BASE_URL` or the default.
pub fn lemonade_base_url() -> String {
    resolve_base_url(std::env::var("LEMONADE_BASE_URL").ok().as_deref())
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
/// Local providers have no key; calling this for one is a programming error,
/// surfaced as an error rather than silently sending an empty key to a server.
pub fn load(provider: Provider) -> Result<String> {
    if provider.is_local() {
        return Err(anyhow!(
            "{provider:?} is a local, keyless provider — use lemonade_base_url(), not load()"
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME is not set; cannot locate key-files"))?;
    let env_value = std::env::var(provider.env_var()).ok();
    resolve(env_value.as_deref(), &provider.key_file(&home))
        .with_context(|| format!("loading {:?} credentials", provider))
}
