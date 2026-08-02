//! ProviderKind credentials, from key-files with an env-var override.
//!
//! Long-term kaibo will take credentials from both files and env. For now the
//! source of truth is a per-provider dotfile in `$HOME`; if the matching env var
//! is set it wins (handy for CI / one-off overrides).
//!
//! - Anthropic:  `ANTHROPIC_API_KEY`  / `~/.anthropic-key.txt`
//! - DeepSeek:   `DEEPSEEK_API_KEY`   / `~/.deepseek-key`
//! - Gemini:     `GEMINI_API_KEY`     / `~/.gemini-api-key`
//! - OpenRouter: `OPENROUTER_API_KEY` / `~/.openrouter-key`
//!
//! [`ProviderKind::Openai`] is the general case: any endpoint speaking the OpenAI
//! wire protocol, addressed by [`openai_base_url`] (`OPENAI_BASE_URL`, default a
//! local keyless server) rather than tied to a hosted service. Its key is
//! *optional* — `OPENAI_API_KEY` / `~/.openai-key` when talking to a keyed
//! endpoint, or a placeholder ([`PLACEHOLDER_OPENAI_KEY`]) for the keyless local
//! default. Per-backend resolution (env → key-file → placeholder) lives on
//! `Backend::resolve_key` (`config.rs`); this module is the pure key/base-url core.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Default OpenAI-compatible endpoint: a local, keyless server (e.g. an OpenAI-
/// compatible host such as AMD's Lemonade serving Gemma). The `/api/v1` suffix
/// matters — rig posts to `{base_url}/chat/completions`. Override with the
/// `OPENAI_BASE_URL` env var (see [`openai_base_url`]).
pub const DEFAULT_OPENAI_BASE_URL: &str = "http://localhost:13305/api/v1";

/// Hosted OpenAI Platform endpoint. A generic `openai` backend pointed here can
/// use OpenAI's first-party API semantics (not just local `/chat/completions`
/// compatibility), such as the Responses API used by current GPT reasoning models.
pub const HOSTED_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// A model provider. The keyed providers (Anthropic/DeepSeek/Gemini/OpenRouter) each
/// speak their own wire protocol and require an API key. [`ProviderKind::Openai`] is
/// the generic OpenAI-compatible endpoint: any base URL speaking that protocol, with
/// an *optional* key — keyless by default, since the default endpoint is local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    DeepSeek,
    Gemini,
    /// OpenRouter's unified gateway: one keyed endpoint (`OPENROUTER_API_KEY`, fixed
    /// base URL) fronting every upstream model. It speaks the OpenAI wire but is a
    /// *keyed* kind with a pinned endpoint (not a configurable base URL), and it
    /// translates a unified `reasoning` param per underlying provider.
    OpenRouter,
    /// Any OpenAI-compatible endpoint, addressed by base URL; key optional.
    /// Defaults to a local keyless server (Gemma via an OpenAI-compatible host).
    Openai,
    /// Stability AI's v2beta image family — the one **production** wire kaibo speaks,
    /// and the odd one out here in a way worth stating: every other kind is a chat
    /// completion API driven through `rig`, while this one is an image API driven
    /// through kaibo's own facade (`src/stability.rs`). It is therefore only ever valid
    /// on an `image` cast slot ([`crate::config::ModelRole::Image`]); a reasoning slot
    /// pointed at a Stability backend is a load error, because there is no completion
    /// model behind it to resolve.
    ///
    /// Deliberately NOT in the built-in backend list: image generation costs real money
    /// per call and needs a key, so it appears only when an operator writes the stanza.
    Stability,
}

impl ProviderKind {
    /// Every kind, in declaration order. Drives the "expected one of…" error text so a
    /// new kind can never be accepted by `FromStr` while staying unlisted in the message
    /// a user actually reads — a hand-maintained list in that string had already drifted
    /// once (`openrouter` shipped before it was named there).
    pub const ALL: [ProviderKind; 6] = [
        Self::Anthropic,
        Self::DeepSeek,
        Self::Gemini,
        Self::OpenRouter,
        Self::Openai,
        Self::Stability,
    ];

    /// Whether a missing credential is tolerated rather than a hard error. Only
    /// the OpenAI-compatible provider is: its default endpoint is a local keyless
    /// server, so an absent key falls back to a placeholder bearer token
    /// ([`PLACEHOLDER_OPENAI_KEY`]). The keyed providers must fail loudly on a missing key.
    pub fn key_optional(self) -> bool {
        matches!(self, ProviderKind::Openai)
    }

    /// The stand-in credential a `key_optional` backend sends when no real key is
    /// configured — transport-shaped, because "keyless" looks different per wire.
    /// Header-bearer kinds (OpenAI `Authorization: Bearer`, Anthropic `x-api-key`)
    /// want a well-formed NON-EMPTY token a keyless server ignores; an empty bearer
    /// is rejected by some clients/servers, hence [`PLACEHOLDER_OPENAI_KEY`].
    /// Gemini authenticates with a `?key=` QUERY param, so its keyless stand-in is
    /// the EMPTY string: rig then emits a bare `?key=`, which an ambient-auth gateway
    /// (e.g. one gated by network identity) accepts, where a non-empty dummy would be
    /// forwarded upstream to Google and rejected (`API_KEY_INVALID`). This is only
    /// reached when the backend is genuinely keyless; a real Gemini key (required by
    /// Google itself) always resolves ahead of this.
    pub fn placeholder_key(self) -> &'static str {
        // Exhaustive on purpose (no `_`): a new provider kind must consciously
        // choose its keyless stand-in by wire, not inherit a default.
        match self {
            // Query-param (`?key=`) auth: keyless == empty, so rig emits a bare
            // `?key=` an ambient-auth gateway accepts.
            ProviderKind::Gemini => "",
            // Header-bearer auth (`Authorization: Bearer` / `x-api-key`): a keyless
            // server ignores the value, but an empty one is rejected by some
            // clients/servers, so send a well-formed non-empty stand-in.
            ProviderKind::Anthropic
            | ProviderKind::DeepSeek
            | ProviderKind::OpenRouter
            | ProviderKind::Openai
            // Never reached: Stability is not `key_optional`, so a missing key is a
            // hard error long before a stand-in is wanted. It is a header-bearer wire,
            // so it takes the same shape as the others if that ever changes.
            | ProviderKind::Stability => PLACEHOLDER_OPENAI_KEY,
        }
    }

    /// The canonical lower-case name of this kind — the wire-protocol id used in
    /// kind listings and error messages, and (for the keyed kinds) the name of the
    /// built-in backend and cast, so a bare `--cast anthropic` resolves to the
    /// built-ins. The OpenAI built-in's *name* diverges from this id — see
    /// [`ProviderKind::builtin_name`].
    pub fn canonical_name(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Openai => "openai",
            ProviderKind::Stability => "stability",
        }
    }

    /// The name of this kind's built-in backend and cast. Equals [`canonical_name`]
    /// for the keyed providers, but the OpenAI-compatible built-in is named
    /// `openai-local`: its default endpoint is a *local* keyless server (Gemma via an
    /// OpenAI-compatible host), so the bare `openai` — which is really the wire
    /// protocol's id — would misrepresent what the built-in points at. `openai` is
    /// deliberately *not* an alias of it, so a user can name their own backend
    /// `[backends.openai]` (a hosted endpoint) without colliding.
    pub fn builtin_name(self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai-local",
            _ => self.canonical_name(),
        }
    }

    /// The environment variable that overrides the key-file.
    pub fn env_var(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
            ProviderKind::DeepSeek => "DEEPSEEK_API_KEY",
            ProviderKind::Gemini => "GEMINI_API_KEY",
            ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
            ProviderKind::Openai => "OPENAI_API_KEY",
            // Reuses the constant `src/stability.rs` already resolves keys with, so the
            // facade and the config layer can never disagree about which var to read.
            ProviderKind::Stability => crate::stability::STABILITY_KEY_ENV_VAR,
        }
    }

    /// The key-file's name within `$HOME`. For the OpenAI provider the key is
    /// optional (see [`ProviderKind::key_optional`]); the file is consulted only if
    /// present.
    pub fn key_file_name(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => ".anthropic-key.txt",
            ProviderKind::DeepSeek => ".deepseek-key",
            ProviderKind::Gemini => ".gemini-api-key",
            ProviderKind::OpenRouter => ".openrouter-key",
            ProviderKind::Openai => ".openai-key",
            // Same constant `src/stability.rs` uses, for the same reason as `env_var`.
            ProviderKind::Stability => crate::stability::STABILITY_KEY_FILE_NAME,
        }
    }

    /// The key-file path under the given home directory.
    pub fn key_file(self, home: &Path) -> PathBuf {
        home.join(self.key_file_name())
    }

    /// Which side of the completion/media line this kind lives on — the structural
    /// gate that keeps media kinds out of the completion-path machinery. Exhaustive
    /// on purpose: a new `ProviderKind` variant fails to compile until it is
    /// classified here, and that one decision is the *only* completion-vs-media
    /// decision the new kind has to make. Everything downstream matches on
    /// [`WireKind`] or [`MediaKind`] and never sees the other family.
    pub fn class(self) -> ProviderClass {
        match self {
            ProviderKind::Anthropic => ProviderClass::Wire(WireKind::Anthropic),
            ProviderKind::DeepSeek => ProviderClass::Wire(WireKind::DeepSeek),
            ProviderKind::Gemini => ProviderClass::Wire(WireKind::Gemini),
            ProviderKind::OpenRouter => ProviderClass::Wire(WireKind::OpenRouter),
            ProviderKind::Openai => ProviderClass::Wire(WireKind::Openai),
            ProviderKind::Stability => ProviderClass::Media(MediaKind::Stability),
        }
    }

    /// The completion wire behind this kind, or `None` for a media kind — the
    /// convenience form of [`class`](Self::class) for the completion entry points
    /// (`ModelShape::resolve`, `ModelCaps::resolve`, `EffortWire::resolve`,
    /// `batch_supported`), each of which answers the media case once at its top
    /// instead of growing a media arm in every internal match.
    pub fn wire(self) -> Option<WireKind> {
        match self.class() {
            ProviderClass::Wire(w) => Some(w),
            ProviderClass::Media(_) => None,
        }
    }

    /// Is this a media-generation kind (no completion model behind it)?
    pub fn is_media(self) -> bool {
        matches!(self.class(), ProviderClass::Media(_))
    }
}

/// A completion wire protocol — the [`ProviderKind`] subset rig builds a
/// `CompletionModel` for. The completion-path machinery (request shaping, effort
/// wires, the vision/transport classifiers, batch eligibility, `Arm` construction)
/// matches on THIS enum, so a media kind can never reach those matches and a new
/// piece of completion machinery needs no media arm at all. Reached only through
/// [`ProviderKind::class`]/[`ProviderKind::wire`], whose exhaustive match is where a
/// new kind gets classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireKind {
    Anthropic,
    DeepSeek,
    Gemini,
    OpenRouter,
    Openai,
}

/// A media-generation API — a backend kaibo drives through its own facade and the
/// [`crate::media::MediaModel`] seam, never through rig's completion machinery. Valid
/// only on media cast slots (`image` today); the load-time role/kind guard in
/// `config.rs` refuses every other pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// Stability AI's v2beta family, via `src/stability.rs`.
    Stability,
}

/// The two families a [`ProviderKind`] can belong to — see [`ProviderKind::class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderClass {
    Wire(WireKind),
    Media(MediaKind),
}

impl std::str::FromStr for ProviderKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(ProviderKind::Anthropic),
            "deepseek" => Ok(ProviderKind::DeepSeek),
            "gemini" | "google" => Ok(ProviderKind::Gemini),
            "openrouter" => Ok(ProviderKind::OpenRouter),
            // The OpenAI-compatible endpoint. Also accept the names people reach
            // for when it points at the local keyless default (Gemma via Lemonade).
            "openai" | "local" | "lemonade" | "gemma" | "gemma4" => Ok(ProviderKind::Openai),
            "stability" => Ok(ProviderKind::Stability),
            other => Err(anyhow!(
                "unknown provider {other:?} (expected one of: {})",
                ProviderKind::ALL
                    .iter()
                    .map(|k| k.canonical_name())
                    .collect::<Vec<_>>()
                    .join(", ")
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

/// Load a *keyed* `provider`'s key from the real environment and `$HOME` (env var
/// over dotfile). The opt-in live-probe tests use it to gate on whether a real key
/// is present. The key-optional (`Openai`) provider is refused: its key may
/// legitimately be absent, so it has no single "the key" to load — resolve it through
/// the backend (`Backend::resolve_key`), which falls back to a placeholder.
pub fn load(provider: ProviderKind) -> Result<String> {
    if provider.key_optional() {
        return Err(anyhow!(
            "{provider:?} tolerates a missing key — load() is for keyed providers only"
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME is not set; cannot locate key-files"))?;
    let env_value = std::env::var(provider.env_var()).ok();
    resolve(env_value.as_deref(), &provider.key_file(&home))
        .with_context(|| format!("loading {:?} credentials", provider))
}
