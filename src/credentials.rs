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
//! default. Per-backend resolution (env → declared file/command → placeholder) lives
//! on `Backend::resolve_key` (`config.rs`), whose sources are ALL declared in
//! config.toml — never seeded — plus [`resolve_key_from_cmd`] for the operator-side
//! command source (`api_key_cmd`, e.g. `op read`). This module is the pure
//! key/base-url core.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

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
    /// OpenAI's Images API (`POST {base}/images/generations`) — the second media kind,
    /// via `src/openai_images.rs`. One kind covers two real targets: hosted OpenAI
    /// image generation (the gpt-image-1 family) and any local server speaking the
    /// same endpoint shape (stable-diffusion.cpp's sd-server). Its key SOURCES
    /// mirror [`ProviderKind::Openai`] (the same `OPENAI_API_KEY` / `~/.openai-key`
    /// — one OpenAI credential, not two to maintain), but the key is REQUIRED by
    /// default: this kind's default endpoint is hosted OpenAI, so a keyless seed
    /// would 401 on the first paid call instead of failing at load (see
    /// [`ProviderKind::key_optional`]). A keyless local sd-server backend sets
    /// `key_optional = true` beside its explicit local `base_url`. Like Stability,
    /// it is not in the built-in backend list and staffs only `image` cast slots.
    OpenAiImages,
    /// Gemini's image generation — the fourth media kind, via `src/gemini_images.rs`.
    ///
    /// A *separate kind* from [`ProviderKind::Gemini`] for the same reason
    /// [`ProviderKind::OpenAiImages`] is separate from [`ProviderKind::Openai`]: one
    /// vendor, two APIs, and a cast slot has to know which one it is pointing at. The
    /// twist here is that Gemini's image generation is not a distinct *endpoint* — it is
    /// `models/{model}:generateContent`, the same call as text, with
    /// `responseModalities` naming what comes back. So this kind is a media kind whose
    /// wire is a completion wire, which is exactly why it needs its own name: a reasoning
    /// slot pointed here would resolve a completion model that answers in pictures.
    GeminiImages,
    /// Alibaba's DashScope multimodal-generation route — the third media kind, via
    /// `src/dashscope.rs`, covering the `wan` image family. Media-only on purpose:
    /// the same host serves text through an OpenAI-compatible endpoint, so a text
    /// model on a DashScope backend belongs on `kind = "openai"` with that base URL,
    /// and this kind stays the one wire kaibo speaks for DashScope image generation.
    ///
    /// This is the kind that made kaibo fetch an artifact URL. DashScope delivers
    /// generated images as presigned links rather than inline bytes, and the CAS
    /// addresses content by its hash — so there is no artifact at all until the bytes
    /// are in hand (see `crate::cas::fetch_artifact_bytes`). Keyed and not built-in,
    /// like the other media kinds: generation costs money per image.
    DashScope,
    /// Black Forest Labs' FLUX image family — the fifth media kind, via
    /// `src/bfl.rs`. One endpoint per named operation (`flux-dev`, `flux-2-pro`, ...),
    /// and every one of them asynchronous: a create call returns a job id and a
    /// `polling_url` to poll — unlike Stability (mostly synchronous) or
    /// DashScope/Gemini (always synchronous). `generate` polls in-call for the
    /// common fast case and falls back to a `job-N` handle only when the provider is
    /// still working past that budget; see `src/bfl.rs`'s module doc. Keyed and not
    /// built-in, like the other media kinds: generation costs money per image.
    Bfl,
}

impl ProviderKind {
    /// Every kind, in declaration order. Drives the "expected one of…" error text so a
    /// new kind can never be accepted by `FromStr` while staying unlisted in the message
    /// a user actually reads — a hand-maintained list in that string had already drifted
    /// once (`openrouter` shipped before it was named there).
    pub const ALL: [ProviderKind; 10] = [
        Self::Anthropic,
        Self::DeepSeek,
        Self::Gemini,
        Self::OpenRouter,
        Self::Openai,
        Self::Stability,
        Self::OpenAiImages,
        Self::GeminiImages,
        Self::DashScope,
        Self::Bfl,
    ];

    /// Whether a missing credential is tolerated rather than a hard error. Only the
    /// OpenAI-compatible *completion* provider is: its default endpoint is a local
    /// keyless server, so an absent key coherently falls back to a placeholder
    /// bearer token ([`PLACEHOLDER_OPENAI_KEY`]). The images kind shares its key
    /// sources but seeds key-REQUIRED, because its default endpoint is hosted
    /// OpenAI: a keyless seed there would let a minimal stanza load clean, staff
    /// `generate`, and then send `Bearer no-auth` to api.openai.com — a 401 on the
    /// first paid call instead of a loud load-time gap. A keyless local sd-server
    /// backend opts in with `key_optional = true` explicitly, beside its explicit
    /// local `base_url`. The keyed providers must fail loudly on a missing key.
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
            // Reached only when an operator sets `key_optional = true` for a local
            // sd-server: header-bearer shaped, value ignored by such a server.
            | ProviderKind::OpenAiImages
            | ProviderKind::GeminiImages
            // Never reached: none of Stability, DashScope, or BFL is `key_optional`,
            // so a missing key is a hard error long before a stand-in is wanted. All
            // three are header-bearer wires, so they take the same shape as the
            // others if that ever changes.
            | ProviderKind::Stability
            | ProviderKind::DashScope
            | ProviderKind::Bfl => PLACEHOLDER_OPENAI_KEY,
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
            ProviderKind::OpenAiImages => "openai-images",
            ProviderKind::GeminiImages => "gemini-images",
            ProviderKind::DashScope => "dashscope",
            ProviderKind::Bfl => "bfl",
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
            ProviderKind::Gemini | ProviderKind::GeminiImages => "GEMINI_API_KEY",
            ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
            // The images kind shares the completion kind's key sources on purpose:
            // both talk to the same OpenAI account when hosted, and both cover a
            // keyless local server when not — one credential, not two to maintain.
            ProviderKind::Openai | ProviderKind::OpenAiImages => "OPENAI_API_KEY",
            // Reuses the constant `src/stability.rs` already resolves keys with, so the
            // facade and the config layer can never disagree about which var to read.
            ProviderKind::Stability => crate::stability::STABILITY_KEY_ENV_VAR,
            // Its own credential, not shared with the `openai` kind: the same host
            // serves text on an OpenAI-compatible route, and an operator who runs both
            // wants one name per account, not one name per protocol.
            ProviderKind::DashScope => crate::dashscope::DASHSCOPE_KEY_ENV_VAR,
            // Reuses the constant `src/bfl.rs` already resolves keys with, so the
            // facade and the config layer can never disagree about which var to read.
            ProviderKind::Bfl => crate::bfl::BFL_KEY_ENV_VAR,
        }
    }

    /// The key-file's name within `$HOME`. For the OpenAI provider the key is
    /// optional (see [`ProviderKind::key_optional`]); the file is consulted only if
    /// present.
    pub fn key_file_name(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => ".anthropic-key.txt",
            ProviderKind::DeepSeek => ".deepseek-key",
            ProviderKind::Gemini | ProviderKind::GeminiImages => ".gemini-api-key",
            ProviderKind::OpenRouter => ".openrouter-key",
            // Shared with the images kind — see `env_var`.
            ProviderKind::Openai | ProviderKind::OpenAiImages => ".openai-key",
            // Same constant `src/stability.rs` uses, for the same reason as `env_var`.
            ProviderKind::Stability => crate::stability::STABILITY_KEY_FILE_NAME,
            // Same reasoning as `env_var`: DashScope's own credential name.
            ProviderKind::DashScope => crate::dashscope::DASHSCOPE_KEY_FILE_NAME,
            // Same constant `src/bfl.rs` uses, for the same reason as `env_var`.
            ProviderKind::Bfl => crate::bfl::BFL_KEY_FILE_NAME,
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
            ProviderKind::OpenAiImages => ProviderClass::Media(MediaKind::OpenAiImages),
            ProviderKind::GeminiImages => ProviderClass::Media(MediaKind::GeminiImages),
            ProviderKind::DashScope => ProviderClass::Media(MediaKind::DashScope),
            ProviderKind::Bfl => ProviderClass::Media(MediaKind::Bfl),
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
    /// OpenAI's Images API shape (`/v1/images/generations`), via
    /// `src/openai_images.rs` — hosted OpenAI or a local sd-server.
    OpenAiImages,
    /// Gemini's `generateContent` asked for image output, via `src/gemini_images.rs`.
    GeminiImages,
    /// Alibaba DashScope's multimodal-generation route, via `src/dashscope.rs` —
    /// the `wan` image family, delivered as presigned URLs kaibo fetches.
    DashScope,
    /// Black Forest Labs' FLUX image family, via `src/bfl.rs` — every operation
    /// asynchronous, delivered as a signed URL kaibo fetches.
    Bfl,
}

/// The two families a [`ProviderKind`] can belong to — see [`ProviderKind::class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderClass {
    Wire(WireKind),
    Media(MediaKind),
}

/// The media-class kinds as a display list (``"`stability`, `openai-images`"``) —
/// derived from [`ProviderKind::ALL`] through [`ProviderKind::class`], so guidance
/// text built at format time never carries a hand-maintained kind list that goes
/// stale when a media kind lands. (Text that must live in a `const` still names the
/// kinds literally; this helper serves every site that formats at runtime.)
pub fn media_kinds_list() -> String {
    ProviderKind::ALL
        .iter()
        .filter(|k| k.is_media())
        .map(|k| format!("`{}`", k.canonical_name()))
        .collect::<Vec<_>>()
        .join(", ")
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
            "openai-images" => Ok(ProviderKind::OpenAiImages),
            "gemini-images" => Ok(ProviderKind::GeminiImages),
            "dashscope" => Ok(ProviderKind::DashScope),
            "bfl" => Ok(ProviderKind::Bfl),
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

/// Cap on a key command's stdout: refuse, not trim, past this. A key is a line;
/// a command that prints a log is misbehaving, not "verbose".
pub const KEY_CMD_MAX_OUTPUT: usize = 64 * 1024;

/// Wall-clock ceiling on a key command. A child that hangs — an unlock prompt (a
/// null stdin means we cannot answer one), a cold agent — must not wedge key
/// resolution. Lazy client-build resolution makes this a pathological-case guard:
/// with an unlocked session, `op read` finishes in a few hundred milliseconds.
pub const KEY_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Run a declared key command and return its stdout as the key.
///
/// `args` is raw argv — no shell, no `$VAR`/`~` expansion: the config layer never
/// interpolates command elements, and neither do we. An empty `args` names no
/// executable — config load already refuses `api_key_cmd = []`, but this is a
/// `pub fn` a direct caller can reach without going through config, so it returns
/// the same loud error rather than panicking on `args[0]`. The child inherits
/// kaibo's environment (where e.g. `op` finds its session or gpg-agent serves
/// `pass`); `extra_env` is the test seam — unit tests inject through it instead of
/// mutating the process environment.
///
/// All three stdio streams are pinned. Stdin is nulled: the child must never read
/// the MCP stdio stream kaibo runs on, and a prompting tool fails fast instead of
/// hanging on a TTY that does not exist. Stdout and stderr are piped and captured —
/// stdout IS the key, so nothing a child prints there can leak into kaibo's own
/// stdout or a log. Stderr is captured too, but never quoted into an error or a log:
/// a wrapper script traced with `set -x`, or a tool that echoes the secret on its
/// own failure path, can put the key on stderr as easily as stdout, so a failure
/// names only the exit status and a byte count, never stderr's content.
///
/// The key is stdout, trimmed (key-file semantics). Empty, non-UTF-8, or
/// over-[`KEY_CMD_MAX_OUTPUT`] output, a nonzero exit, a spawn failure, and a
/// timeout are all loud errors — the no-silent-fallback rule: a declared-but-broken
/// command never degrades to a placeholder.
///
/// No caching: this runs fresh on every call, and [`crate::config::Backend::resolve_key`]
/// has no memoization either, so a caller that builds a client per cast role —
/// `consult`'s synth and explorer, say — runs this command once per role, not once
/// per top-level call.
pub fn resolve_key_from_cmd(
    args: &[String],
    timeout: Duration,
    extra_env: &[(&str, &str)],
) -> Result<String> {
    let Some(program) = args.first() else {
        return Err(anyhow!(
            "api_key_cmd names no executable — give it at least the command to \
             run (`kaibo example-config` shows the shape)"
        ));
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !extra_env.is_empty() {
        cmd.envs(extra_env.iter().copied());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning key command {:?}", program))?;
    let stdout = child
        .stdout
        .take()
        .context("key command stdout not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("key command stderr not piped")?;

    // Drain both pipes on reader threads so a verbose child can never wedge the
    // pipe; `read_capped` buffers only the first ~cap bytes and drains the rest.
    let reader_stdout = std::thread::spawn(move || read_capped(stdout, KEY_CMD_MAX_OUTPUT));
    let reader_stderr = std::thread::spawn(move || read_capped(stderr, KEY_CMD_MAX_OUTPUT));

    // Wait for exit under a hard deadline; kill and report on timeout.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "key command {:?} did not exit within {:.1}s and was killed — \
                     stdin is closed, so an interactive unlock prompt cannot be \
                     answered (unlock the tool once, or point it at a key that \
                     needs no prompt)",
                    program,
                    timeout.as_secs_f64(),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(e).context("waiting for the key command"),
        }
    };

    let out = reader_stdout
        .join()
        .map_err(|_| anyhow!("key command stdout reader panicked"))?;
    let err = reader_stderr
        .join()
        .map_err(|_| anyhow!("key command stderr reader panicked"))?;

    // A nonzero exit is the primary failure: name it and how much stderr the
    // child wrote, never stderr's content — stderr can carry the secret itself
    // (a wrapper script run with `set -x`, or a tool that echoes on its own
    // failure path), so it is suppressed exactly where stdout already is.
    if !status.success() {
        return Err(anyhow!(
            "key command {:?} exited with {status} ({} bytes on stderr, \
             suppressed — stderr can carry the secret itself): run the command \
             directly to see why it failed, then fix it or the secret it reads \
             (its stdout must be the key)",
            program,
            err.len(),
        ));
    }

    // Refuse, not trim: a key is a single value, not a log.
    if out.len() > KEY_CMD_MAX_OUTPUT {
        return Err(anyhow!(
            "key command {:?} printed more than {} bytes of output — a key is a \
             single value; point the command at a field that prints one",
            program,
            KEY_CMD_MAX_OUTPUT,
        ));
    }

    // Strict UTF-8, never lossy: a binary field (an `op read` of a file-type
    // item) is a mistake to surface, not to mangle into a key.
    let out = String::from_utf8(out).with_context(|| {
        format!(
            "key command {:?} printed non-UTF-8 output — the key must be text \
             (a base64'd binary field? wrap it in a script that decodes)",
            program
        )
    })?;
    let key = out.trim();
    if key.is_empty() {
        return Err(anyhow!(
            "key command {:?} printed nothing — the command must print the key \
             to stdout",
            program
        ));
    }
    Ok(key.to_string())
}

/// Read a pipe to EOF, buffering roughly the first `cap` bytes and draining the
/// rest — a stopped reader would wedge the child's pipe, turning a verbose child
/// into a hang instead of a loud refusal.
fn read_capped(mut r: impl Read, cap: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cap.min(65536));
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return buf,
            Ok(n) => {
                if buf.len() <= cap {
                    let room = cap + 1 - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            // A signal can interrupt a read; retry it, the child is still ours.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return buf,
        }
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
