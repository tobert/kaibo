//! Provider credentials, from key-files with an env-var override.
//!
//! Long-term kaibo will take credentials from both files and env. For now the
//! source of truth is a per-provider dotfile in `$HOME`; if the matching env var
//! is set it wins (handy for CI / one-off overrides).
//!
//! - Anthropic: `ANTHROPIC_API_KEY` / `~/.anthropic-key.txt`
//! - DeepSeek:  `DEEPSEEK_API_KEY`  / `~/.deepseek-key`
//! - Gemini:    `GEMINI_API_KEY`    / `~/.gemini-api-key`

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// A credentialed model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    DeepSeek,
    Gemini,
}

impl Provider {
    /// The environment variable that overrides the key-file.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
        }
    }

    /// The key-file's name within `$HOME`.
    pub fn key_file_name(self) -> &'static str {
        match self {
            Provider::Anthropic => ".anthropic-key.txt",
            Provider::DeepSeek => ".deepseek-key",
            Provider::Gemini => ".gemini-api-key",
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
            other => Err(anyhow!(
                "unknown provider {other:?} (expected anthropic, deepseek, or gemini)"
            )),
        }
    }
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
pub fn load(provider: Provider) -> Result<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME is not set; cannot locate key-files"))?;
    let env_value = std::env::var(provider.env_var()).ok();
    resolve(env_value.as_deref(), &provider.key_file(&home))
        .with_context(|| format!("loading {:?} credentials", provider))
}
