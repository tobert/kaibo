//! kaibo's configuration: a registry of named provider *profiles* plus server and
//! tunable defaults, layered CLI > env > file > built-in.
//!
//! The load-bearing idea is the split between a [`ProviderKind`] (the wire
//! protocol — the only thing that picks a rig client and `thinking_params`) and a
//! [`Profile`] (a *named instance* of a kind, with its own base URL, key source,
//! and models). That split is what lets two OpenAI-compatible endpoints — hosted
//! GPT and a local llama.cpp/Gemma server, say — both be live at once, each
//! selected by profile name. The old enum-as-selector `Provider` couldn't express
//! that. See `docs/config.md`.
//!
//! ## Layering
//!
//! Built-in profiles (the four below) ship in code and reproduce kaibo's historical
//! behavior, so a **missing config file is not an error**. A `config.toml` *merges
//! over* them: set one field to retarget a built-in, or add a wholly new profile.
//! `KAIBO_*` env vars override the file; CLI flags (applied in `main`) override env.
//!
//! ## Loud over silent
//!
//! Per Amy's directive, a misconfiguration crashes rather than degrading quietly:
//! malformed TOML, an unknown key (`deny_unknown_fields`), a `base_url` on a keyed
//! kind, a new profile with no `kind`, an unresolvable `default_provider`, or an
//! alias that collides with a profile name are all hard errors at load.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::credentials::{self, ProviderKind, PLACEHOLDER_OPENAI_KEY};
use crate::sandbox::SandboxConfig;
use crate::server::ToolGating;

// --- Tunable defaults (the [defaults] table) -------------------------------

/// Loop/budget tunables shared by every profile. `max_tokens` and `thinking_budget`
/// are *also* overridable per profile (they track the model, not the server); the
/// turn caps stay here and per-call (they bound the loop, not the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub explorer_max_turns: usize,
    pub synth_max_turns: usize,
    pub max_tokens: u64,
    pub thinking_budget: u64,
    /// Per-request deadline on a single LLM completion call (a per-*HTTP-call*
    /// bound, not the whole loop). Seeds every profile's `request_timeout`;
    /// overridable per profile (a slow local model wants more rope than a hosted
    /// API). See [`Profile::request_timeout`].
    pub request_timeout: Duration,
    /// Max distinct multi-turn `consult` sessions held in memory at once. Eviction
    /// is capacity-driven only (no TTL) — see [`crate::session`]. Server-wide, not
    /// per profile (a session is a client thread, not a model trait).
    pub session_capacity: NonZeroUsize,
}

impl Default for Defaults {
    fn default() -> Self {
        // Mirror the historical consult defaults exactly (see the old
        // ConsultConfig::default + THINKING_BUDGET) so a config-less run is
        // byte-for-byte the prior behavior.
        Self {
            explorer_max_turns: 50,
            synth_max_turns: 100,
            max_tokens: 16384,
            thinking_budget: crate::consult::THINKING_BUDGET,
            // 15 min. A single completion that takes longer is pathological for a
            // hosted API and a generous ceiling for a slow local model; either way
            // it bounds a wedged provider that would otherwise hang forever
            // (non-streaming loop → no other brake). Tune per profile if needed.
            request_timeout: Duration::from_secs(900),
            // 128 lean Q&A threads is a few KB of strings — generous for a personal
            // server, and capacity (not time) is the only eviction pressure.
            session_capacity: NonZeroUsize::new(128).expect("128 is nonzero"),
        }
    }
}

// --- A resolved profile ----------------------------------------------------

/// A fully-resolved provider profile: a named instance of a [`ProviderKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The profile's name (the value the `provider` arg carries).
    pub name: String,
    /// Which wire protocol / rig client to construct.
    pub kind: ProviderKind,
    /// Endpoint base URL. Meaningful only for `kind = Openai`; a `Some` on any keyed
    /// kind is rejected at load (rig fixes those endpoints).
    pub base_url: Option<String>,
    /// Env var to read the API key from (checked before `api_key_file`).
    pub api_key_env: Option<String>,
    /// Key-file path (`~` expanded). Used when the env var is unset/blank.
    pub api_key_file: Option<String>,
    /// When true, a missing key falls back to a placeholder bearer token instead of
    /// erroring (the keyless local-server case).
    pub key_optional: bool,
    pub explorer_model: String,
    pub synth_model: String,
    pub max_tokens: u64,
    pub thinking_budget: u64,
    /// Per-request deadline applied to this profile's HTTP client (`.timeout`):
    /// the wall-clock ceiling on a single completion call. rig's prompt loop is
    /// non-streaming and exposes no native timeout, so without this a provider
    /// that connects but never responds wedges the call indefinitely — exactly
    /// the 2026-06-06 stall (see `consult.rs` / `docs/issues.md`). Seeded from
    /// [`Defaults::request_timeout`], overridable per profile.
    pub request_timeout: Duration,
}

impl Profile {
    /// Resolve this profile's bearer token: configured env var (wins, when set and
    /// non-blank), then the key file, then — if `key_optional` — a placeholder, else
    /// a loud error.
    ///
    /// A *present but broken* key file (empty, unreadable, a directory) is a loud
    /// error even for a `key_optional` profile: a key that's there but wrong is a
    /// mistake, not "keyless". Only a genuinely absent file falls back. This is the
    /// no-silent-fallback directive — we don't quietly send a placeholder when the
    /// user clearly meant to provide a key.
    pub fn resolve_key(&self) -> Result<String> {
        // env wins, when set and non-blank.
        if let Some(v) = self
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
        {
            let v = v.trim();
            if !v.is_empty() {
                return Ok(v.to_string());
            }
        }

        // then a configured key file, *if it exists*.
        if let Some(file) = self.api_key_file.as_deref().map(expand_tilde) {
            if file.exists() {
                // env already handled above, so pass None — `resolve` reads the file
                // and errors loudly on empty/unreadable (for keyed AND keyless).
                return credentials::resolve(None, &file)
                    .with_context(|| format!("resolving key for profile {:?}", self.name));
            }
        }

        // No env, no existing file.
        if self.key_optional {
            Ok(PLACEHOLDER_OPENAI_KEY.to_string())
        } else {
            Err(anyhow!(
                "profile {:?} has no API key: env {} unset and key file {} absent — \
                 set one, or key_optional = true only for a keyless endpoint",
                self.name,
                self.api_key_env.as_deref().unwrap_or("(none)"),
                self.api_key_file.as_deref().unwrap_or("(none)"),
            ))
        }
    }

    /// The base URL to dial for an OpenAI-compatible client. An explicit per-profile
    /// `base_url` wins; otherwise fall back to `OPENAI_BASE_URL` (back-compat) or the
    /// built-in default. Read at use-time, not construction, so profile *building*
    /// stays pure (see [`Config::from_toml_str`]).
    pub fn resolved_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(credentials::openai_base_url)
    }
}

// --- The whole config ------------------------------------------------------

/// kaibo's resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Default project root when a call omits `path` (`--root` / `KAIBO_ROOT`).
    pub root: Option<PathBuf>,
    /// Default profile name when a call omits `provider`.
    pub default_provider: String,
    /// `EnvFilter` directive used when `RUST_LOG` is unset.
    pub log: String,
    /// Which tools to advertise.
    pub tools: ToolGating,
    pub defaults: Defaults,
    /// Read-only sandbox limits (exec timeout, output cap, extra disabled builtins).
    pub sandbox: SandboxConfig,
    /// Profiles by name.
    pub profiles: BTreeMap<String, Profile>,
    /// alias → profile name.
    aliases: BTreeMap<String, String>,
}

impl Config {
    /// The built-in registry with no config file: kaibo's historical behavior.
    pub fn builtin() -> Self {
        // RawConfig::default() merges to exactly the built-ins; unwrap is sound
        // because the built-in registry is internally consistent by construction.
        Self::merge(RawConfig::default()).expect("built-in config must be valid")
    }

    /// Load from `path` if given (must exist), else the default XDG location (absent
    /// is fine → built-ins). Applies `KAIBO_*` env overrides on top of the file.
    pub fn load(explicit_path: Option<PathBuf>) -> Result<Self> {
        Self::load_with(explicit_path, default_config_path(), |k| std::env::var(k).ok())
    }

    /// Testable core of [`load`]: the XDG default and env lookup are injected.
    pub fn load_with(
        explicit_path: Option<PathBuf>,
        default_path: Option<PathBuf>,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let (path, required) = match explicit_path {
            Some(p) => (Some(p), true),
            None => (default_path, false),
        };

        let mut raw = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str::<RawConfig>(&text)
                    .with_context(|| format!("parsing config {}", p.display()))?
            }
            Some(p) if required => {
                bail!("config file not found: {}", p.display())
            }
            _ => RawConfig::default(),
        };

        // env over file, under CLI: fold KAIBO_* into the raw tables before merge so
        // env'd defaults flow into the profiles that inherit them.
        apply_raw_env(&mut raw, &get_env)?;
        Self::merge(raw)
    }

    /// Parse a config from a TOML string with **no** env or filesystem access — the
    /// pure entry point tests drive for merge precedence and validation.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(s).context("parsing config")?;
        Self::merge(raw)
    }

    /// Resolve a profile by name or alias. An unknown name is a loud error naming
    /// the available profiles — the client asked for something we can't serve.
    pub fn resolve_profile(&self, name: &str) -> Result<&Profile> {
        if let Some(p) = self.profiles.get(name) {
            return Ok(p);
        }
        if let Some(real) = self.aliases.get(name) {
            return Ok(&self.profiles[real]);
        }
        let mut names: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
        names.sort_unstable();
        Err(anyhow!(
            "unknown provider/profile {name:?}; known profiles: {}",
            names.join(", ")
        ))
    }

    /// Validate `[sandbox].disable_builtins` against the set of builtins actually
    /// compiled in (`known`). An unknown name is a loud startup error — a typo must
    /// not silently leave a builtin enabled. Call with [`crate::sandbox::builtin_names`].
    pub fn validate_against_builtins(&self, known: &[String]) -> Result<()> {
        for name in &self.sandbox.disable_builtins {
            if !known.iter().any(|k| k == name) {
                let mut sorted: Vec<&str> = known.iter().map(String::as_str).collect();
                sorted.sort_unstable();
                bail!(
                    "[sandbox].disable_builtins names {name:?}, which is not a kaish \
                     builtin in this build; known builtins: {}",
                    sorted.join(", ")
                );
            }
        }
        Ok(())
    }

    /// Merge a raw (file/env) config over the built-in registry and validate.
    fn merge(raw: RawConfig) -> Result<Self> {
        let defaults = merge_defaults(raw.defaults.unwrap_or_default())?;

        // Start from the built-in profiles, then apply the file's profile table.
        // Collect file-declared aliases as we go (the alias map is built once all
        // profiles exist, so collisions can be checked against the full set).
        let mut profiles = builtin_profiles(&defaults);
        let mut file_aliases: Vec<(String, String)> = Vec::new();
        for (name, rp) in raw.profiles.unwrap_or_default() {
            match profiles.get_mut(&name) {
                // Overriding a built-in (or an earlier-defined) profile.
                Some(existing) => rp.apply_to(existing)?,
                // A new profile: seed from its kind's built-in template, then apply.
                None => {
                    let kind = rp.kind.as_deref().ok_or_else(|| {
                        anyhow!("profile {name:?} is new and must declare a `kind`")
                    })?;
                    let kind: ProviderKind = kind
                        .parse()
                        .with_context(|| format!("profile {name:?} kind"))?;
                    let mut p = template_for_kind(kind, &defaults);
                    p.name = name.clone();
                    rp.apply_to(&mut p)?;
                    profiles.insert(name.clone(), p);
                }
            }
            for alias in rp.aliases.into_iter().flatten() {
                file_aliases.push((alias, name.clone()));
            }
        }

        // Validate each profile. These are config mistakes, not no-ops: crash loudly
        // at startup rather than fail cryptically mid-call.
        for p in profiles.values() {
            // A base_url on a keyed kind: rig fixes those endpoints.
            if p.kind != ProviderKind::Openai && p.base_url.is_some() {
                bail!(
                    "profile {:?} (kind {:?}) sets base_url, but only the `openai` kind \
                     has a configurable endpoint",
                    p.name,
                    p.kind
                );
            }
            // Thinking-on kinds need output headroom above the reasoning budget;
            // Anthropic *requires* max_tokens > budget_tokens (see consult.rs). Catch
            // an inverted config here instead of as a 400 on the first call.
            if matches!(p.kind, ProviderKind::Anthropic | ProviderKind::Gemini)
                && p.thinking_budget >= p.max_tokens
            {
                bail!(
                    "profile {:?}: thinking_budget ({}) must be < max_tokens ({}) — \
                     reasoning would starve the answer (Anthropic rejects it outright)",
                    p.name,
                    p.thinking_budget,
                    p.max_tokens
                );
            }
            // A zero deadline means "time out instantly" — never a real intent, and
            // it would brick every call. Catch it at load, not as a mystery failure
            // on the first request. (There is no "disable" escape hatch: an infinite
            // wait is the bug this field exists to prevent.)
            if p.request_timeout.is_zero() {
                bail!(
                    "profile {:?}: request_timeout_secs must be > 0 — a zero deadline \
                     times out every call instantly",
                    p.name,
                );
            }
        }

        // Build the alias map and reject collisions loudly: built-in aliases first,
        // then file-declared ones.
        let mut aliases: BTreeMap<String, String> = BTreeMap::new();
        for name in profiles.keys() {
            for alias in builtin_aliases(name) {
                register_alias(&mut aliases, &profiles, alias, name)?;
            }
        }
        for (alias, name) in file_aliases {
            register_alias(&mut aliases, &profiles, alias, &name)?;
        }

        let server = raw.server.unwrap_or_default();
        let tools = merge_tools(server.tools.unwrap_or_default());
        let default_provider = server.provider.unwrap_or_else(|| "anthropic".to_string());
        let log = server.log.unwrap_or_else(|| "kaibo=info".to_string());
        let root = server.root.map(PathBuf::from);
        let sandbox = merge_sandbox(raw.sandbox.unwrap_or_default());

        let cfg = Self {
            root,
            default_provider,
            log,
            tools,
            defaults,
            sandbox,
            profiles,
            aliases,
        };

        // default_provider must resolve now (CLI may still override it; main
        // re-validates the final choice). Catch a typo in the file early.
        cfg.resolve_profile(&cfg.default_provider).with_context(|| {
            format!(
                "config `server.provider` = {:?} names no profile",
                cfg.default_provider
            )
        })?;
        Ok(cfg)
    }

    /// Apply CLI overrides (highest precedence). Called from `main` after load.
    /// CLI can only *disable* a tool (the `--no-<tool>` flags); enabling is the job
    /// of the file/env/built-in layers.
    pub fn apply_cli(
        &mut self,
        root: Option<PathBuf>,
        provider: Option<String>,
        disable: ToolDisables,
    ) {
        if let Some(root) = root {
            self.root = Some(root);
        }
        if let Some(provider) = provider {
            self.default_provider = provider;
        }
        if disable.consult {
            self.tools.consult = false;
        }
        if disable.explore {
            self.tools.explore = false;
        }
        if disable.synthesize {
            self.tools.synthesize = false;
        }
        if disable.run_kaish {
            self.tools.run_kaish = false;
        }
    }
}

/// The `--no-<tool>` CLI flags: `true` means "the user asked to drop this tool".
/// A distinct type from [`ToolGating`] (where `true` means *enabled*) so the
/// inverted meaning can't be confused at a call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolDisables {
    pub consult: bool,
    pub explore: bool,
    pub synthesize: bool,
    pub run_kaish: bool,
}

/// Register `alias → name`, rejecting a clash with a real profile name or a
/// different existing alias target.
fn register_alias(
    aliases: &mut BTreeMap<String, String>,
    profiles: &BTreeMap<String, Profile>,
    alias: String,
    name: &str,
) -> Result<()> {
    if profiles.contains_key(&alias) {
        bail!("alias {alias:?} (for {name:?}) collides with a profile of the same name");
    }
    if let Some(prev) = aliases.get(&alias) {
        if prev != name {
            bail!("alias {alias:?} is claimed by both {prev:?} and {name:?}");
        }
    }
    aliases.insert(alias, name.to_string());
    Ok(())
}

// --- Built-in registry -----------------------------------------------------

/// The built-in aliases for a built-in profile name (empty for new profiles —
/// file-declared aliases ride along on the RawProfile and are merged separately).
fn builtin_aliases(name: &str) -> Vec<String> {
    let v: &[&str] = match name {
        "anthropic" => &["claude"],
        "gemini" => &["google"],
        "openai" => &["local", "lemonade", "gemma", "gemma4"],
        _ => &[],
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// A fresh profile template for `kind`, carrying that kind's default key source and
/// models. New file profiles start here, then apply their overrides.
fn template_for_kind(kind: ProviderKind, defaults: &Defaults) -> Profile {
    let (explorer_model, synth_model) = default_models(kind);
    Profile {
        name: String::new(),
        kind,
        // No base_url baked in: `resolved_base_url` supplies the default (or
        // OPENAI_BASE_URL, for the openai kind) at use-time, keeping construction
        // pure. A keyed kind with an explicit base_url is rejected in `merge`.
        base_url: None,
        api_key_env: Some(kind.env_var().to_string()),
        api_key_file: Some(format!("~/{}", kind.key_file_name())),
        key_optional: kind.key_optional(),
        explorer_model: explorer_model.to_string(),
        synth_model: synth_model.to_string(),
        max_tokens: defaults.max_tokens,
        thinking_budget: defaults.thinking_budget,
        request_timeout: defaults.request_timeout,
    }
}

/// The four built-in profiles, named after their kind.
fn builtin_profiles(defaults: &Defaults) -> BTreeMap<String, Profile> {
    let mut m = BTreeMap::new();
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::Openai,
    ] {
        let mut p = template_for_kind(kind, defaults);
        p.name = kind.canonical_name().to_string();
        m.insert(p.name.clone(), p);
    }
    m
}

/// Default (explorer, synth) model ids per kind. The seed values for the built-in
/// registry; they drift — keep in sync with the source-of-truth pal configs
/// (`provider-model-ids` memory).
pub fn default_models(kind: ProviderKind) -> (&'static str, &'static str) {
    match kind {
        ProviderKind::Anthropic => ("claude-haiku-4-5", "claude-sonnet-4-6"),
        ProviderKind::DeepSeek => ("deepseek-v4-flash", "deepseek-v4-pro"),
        ProviderKind::Gemini => ("gemini-flash-lite-latest", "gemini-3.5-flash"),
        ProviderKind::Openai => ("Gemma-4-E4B-it-GGUF", "Gemma-4-26B-A4B-it-GGUF"),
    }
}

// --- Raw (deserialized) shapes ---------------------------------------------

/// The on-disk shape. Everything optional (it overrides built-ins);
/// `deny_unknown_fields` makes a typo'd key a load error, not a silent no-op.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: Option<RawServer>,
    defaults: Option<RawDefaults>,
    sandbox: Option<RawSandbox>,
    profiles: Option<BTreeMap<String, RawProfile>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    exec_timeout_secs: Option<u64>,
    output_limit_bytes: Option<usize>,
    /// Builtins to disable on top of the read-only denylist (file-only; no env).
    disable_builtins: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    root: Option<String>,
    provider: Option<String>,
    log: Option<String>,
    tools: Option<RawTools>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTools {
    consult: Option<bool>,
    explore: Option<bool>,
    synthesize: Option<bool>,
    run_kaish: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    explorer_max_turns: Option<usize>,
    synth_max_turns: Option<usize>,
    max_tokens: Option<u64>,
    thinking_budget: Option<u64>,
    request_timeout_secs: Option<u64>,
    session_capacity: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    kind: Option<String>,
    aliases: Option<Vec<String>>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    key_optional: Option<bool>,
    explorer_model: Option<String>,
    synth_model: Option<String>,
    max_tokens: Option<u64>,
    thinking_budget: Option<u64>,
    request_timeout_secs: Option<u64>,
}

impl RawProfile {
    /// Overlay this raw profile's set fields onto a resolved [`Profile`]. A `kind`
    /// that disagrees with the target's existing kind is a loud error (you don't
    /// change a profile's protocol by re-listing it).
    fn apply_to(&self, p: &mut Profile) -> Result<()> {
        if let Some(kind) = &self.kind {
            let kind: ProviderKind = kind
                .parse()
                .with_context(|| format!("profile {:?} kind", p.name))?;
            if kind != p.kind {
                bail!(
                    "profile {:?} declares kind {:?} but already exists as kind {:?}",
                    p.name,
                    kind,
                    p.kind
                );
            }
        }
        if let Some(v) = &self.base_url {
            p.base_url = Some(v.clone());
        }
        if let Some(v) = &self.api_key_env {
            p.api_key_env = Some(v.clone());
        }
        if let Some(v) = &self.api_key_file {
            p.api_key_file = Some(v.clone());
        }
        if let Some(v) = self.key_optional {
            p.key_optional = v;
        }
        if let Some(v) = &self.explorer_model {
            p.explorer_model = v.clone();
        }
        if let Some(v) = &self.synth_model {
            p.synth_model = v.clone();
        }
        if let Some(v) = self.max_tokens {
            p.max_tokens = v;
        }
        if let Some(v) = self.thinking_budget {
            p.thinking_budget = v;
        }
        if let Some(v) = self.request_timeout_secs {
            p.request_timeout = Duration::from_secs(v);
        }
        Ok(())
    }
}

fn merge_defaults(raw: RawDefaults) -> Result<Defaults> {
    let d = Defaults::default();
    // A zero session_capacity can't make a valid LruCache and would mean "remember
    // nothing", which is what omitting `session_id` already does — so it's never a
    // real intent. Reject it at load rather than panicking on the first session.
    let session_capacity = match raw.session_capacity {
        Some(n) => NonZeroUsize::new(n)
            .ok_or_else(|| anyhow!("[defaults] session_capacity must be > 0 (got 0)"))?,
        None => d.session_capacity,
    };
    Ok(Defaults {
        explorer_max_turns: raw.explorer_max_turns.unwrap_or(d.explorer_max_turns),
        synth_max_turns: raw.synth_max_turns.unwrap_or(d.synth_max_turns),
        max_tokens: raw.max_tokens.unwrap_or(d.max_tokens),
        thinking_budget: raw.thinking_budget.unwrap_or(d.thinking_budget),
        request_timeout: raw
            .request_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(d.request_timeout),
        session_capacity,
    })
}

fn merge_sandbox(raw: RawSandbox) -> SandboxConfig {
    let d = SandboxConfig::default();
    SandboxConfig {
        exec_timeout: raw
            .exec_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(d.exec_timeout),
        output_limit_bytes: raw.output_limit_bytes.unwrap_or(d.output_limit_bytes),
        disable_builtins: raw.disable_builtins.unwrap_or(d.disable_builtins),
    }
}

fn merge_tools(raw: RawTools) -> ToolGating {
    let d = ToolGating::default();
    ToolGating {
        consult: raw.consult.unwrap_or(d.consult),
        explore: raw.explore.unwrap_or(d.explore),
        synthesize: raw.synthesize.unwrap_or(d.synthesize),
        run_kaish: raw.run_kaish.unwrap_or(d.run_kaish),
    }
}

// --- Env overrides ---------------------------------------------------------

/// Fold `KAIBO_*` env vars into the raw config (env over file). Numeric parse
/// failures are loud — a `KAIBO_MAX_TOKENS=abc` is a mistake, not a fallback.
fn apply_raw_env(raw: &mut RawConfig, get: &impl Fn(&str) -> Option<String>) -> Result<()> {
    let server = raw.server.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_ROOT") {
        server.root = Some(v);
    }
    if let Some(v) = get("KAIBO_PROVIDER") {
        server.provider = Some(v);
    }
    if let Some(v) = get("KAIBO_LOG") {
        server.log = Some(v);
    }
    let tools = server.tools.get_or_insert_with(Default::default);
    if env_flag(get, "KAIBO_NO_CONSULT") {
        tools.consult = Some(false);
    }
    if env_flag(get, "KAIBO_NO_EXPLORE") {
        tools.explore = Some(false);
    }
    if env_flag(get, "KAIBO_NO_SYNTHESIZE") {
        tools.synthesize = Some(false);
    }
    if env_flag(get, "KAIBO_NO_RUN_KAISH") {
        tools.run_kaish = Some(false);
    }

    let defaults = raw.defaults.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_EXPLORER_MAX_TURNS") {
        defaults.explorer_max_turns = Some(parse_env("KAIBO_EXPLORER_MAX_TURNS", &v)?);
    }
    if let Some(v) = get("KAIBO_SYNTH_MAX_TURNS") {
        defaults.synth_max_turns = Some(parse_env("KAIBO_SYNTH_MAX_TURNS", &v)?);
    }
    if let Some(v) = get("KAIBO_MAX_TOKENS") {
        defaults.max_tokens = Some(parse_env("KAIBO_MAX_TOKENS", &v)?);
    }
    if let Some(v) = get("KAIBO_THINKING_BUDGET") {
        defaults.thinking_budget = Some(parse_env("KAIBO_THINKING_BUDGET", &v)?);
    }
    if let Some(v) = get("KAIBO_REQUEST_TIMEOUT_SECS") {
        defaults.request_timeout_secs = Some(parse_env("KAIBO_REQUEST_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_SESSION_CAPACITY") {
        defaults.session_capacity = Some(parse_env("KAIBO_SESSION_CAPACITY", &v)?);
    }

    let sandbox = raw.sandbox.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_EXEC_TIMEOUT_SECS") {
        sandbox.exec_timeout_secs = Some(parse_env("KAIBO_EXEC_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_OUTPUT_LIMIT_BYTES") {
        sandbox.output_limit_bytes = Some(parse_env("KAIBO_OUTPUT_LIMIT_BYTES", &v)?);
    }
    // disable_builtins is a list — file-only, no env form.
    Ok(())
}

fn parse_env<T: std::str::FromStr>(name: &str, v: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    v.trim()
        .parse()
        .map_err(|e| anyhow!("{name}={v:?} is not a valid number: {e}"))
}

/// A `KAIBO_NO_*` flag is on for any non-empty, non-"0"/"false" value.
fn env_flag(get: &impl Fn(&str) -> Option<String>, name: &str) -> bool {
    match get(name) {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        None => false,
    }
}

// --- Paths -----------------------------------------------------------------

/// The default config path: `$XDG_CONFIG_HOME/kaibo/config.toml`, else
/// `~/.config/kaibo/config.toml`. `None` if neither var is set.
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("kaibo").join("config.toml"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config").join("kaibo").join("config.toml"))
}

/// Expand a leading `~` to `$HOME`. Leaves other paths untouched.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}
