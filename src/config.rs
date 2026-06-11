//! kaibo's configuration: connections, compositions, and tunable defaults,
//! layered CLI > env > file > built-in.
//!
//! The load-bearing idea is the split between a [`Backend`] (a *connection*: a
//! [`ProviderKind`] wire protocol plus base URL, key source, and request timeout)
//! and a [`Cast`] (a *composition*: a named assignment of models to
//! [`ModelRole`]s, freely spanning backends). Calls pick casts; backends are
//! reachable only through a cast's slots. That indirection is what lets "deepseek
//! explorer, claude synth, local image gen" be one named thing — the old fused
//! `Profile` couldn't say it. See `docs/casts.md` (the contract for this split).
//!
//! ## Layering
//!
//! Built-in backends and casts (the four kinds, same-named) ship in code and
//! reproduce kaibo's historical behavior, so a **missing config file is not an
//! error**. A `config.toml` *merges over* them: set one field to retarget a
//! built-in, or add a wholly new backend or cast. `KAIBO_*` env vars override the
//! file; CLI flags (applied in `main`) override env.
//!
//! ## Loud over silent
//!
//! Per Amy's directive, a misconfiguration crashes rather than degrading quietly:
//! malformed TOML, an unknown key (`deny_unknown_fields`), a `base_url` on a keyed
//! kind, a slot naming an unknown backend, a leftover `[profiles]` table (deleted,
//! not deprecated — see `docs/casts.md`), an unresolvable `server.cast`, or an
//! alias that collides with a real name are all hard errors at load.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::consult::{ModelCaps, ThinkingStyleOverride};
use crate::credentials::{self, ProviderKind, PLACEHOLDER_OPENAI_KEY};
use crate::sandbox::SandboxConfig;
use crate::server::ToolGating;

// --- Tunable defaults (the [defaults] table) -------------------------------

/// Loop/budget tunables shared by every cast. The model-tracking knobs
/// (`max_tokens`, `thinking_budget`, temperature, effort, `thinking_style`) are
/// *also* overridable per cast slot (they track the model, not the server); the
/// turn caps stay here and per-call (they bound the loop, not the model).
// Not `Eq`: temperatures are `f64`. `PartialEq` is enough for the tests.
#[derive(Debug, Clone, PartialEq)]
pub struct Defaults {
    pub explorer_max_turns: usize,
    pub synth_max_turns: usize,
    pub max_tokens: u64,
    pub thinking_budget: u64,
    /// Sampling temperature per role: the explorer gathers exact citations, so it
    /// runs cold (deterministic); the synth composes the answer, so it gets a touch
    /// more room. Sent to every provider that accepts it (top-level for Anthropic/
    /// DeepSeek/OpenAI, under `generationConfig` for Gemini). Overridable per slot.
    pub explorer_temperature: f64,
    pub synth_temperature: f64,
    /// Nucleus sampling, both roles. Mild by default; the temperature is the main
    /// lever.
    pub top_p: f64,
    /// Per-role reasoning effort for the models that take one as a request param
    /// (Anthropic adaptive's `output_config.effort`, DeepSeek's `reasoning_effort`).
    /// A passthrough string — the provider validates it. Default `"high"` both roles;
    /// bump a synth slot's `effort` to `"max"`/`"xhigh"` for heavier synth runs.
    pub explorer_effort: String,
    pub synth_effort: String,
    /// Force the Anthropic thinking style (`auto`/`adaptive`/`budget`) instead of the
    /// built-in classifier — the escape hatch for a new or misclassified model.
    /// Server-wide default; overridable per slot. A no-op for non-Anthropic kinds.
    pub thinking_style: ThinkingStyleOverride,
    /// Per-request deadline on a single LLM completion call (a per-*HTTP-call*
    /// bound, not the whole loop). Seeds every backend's `request_timeout`;
    /// overridable per backend (a slow local model wants more rope than a hosted
    /// API). See [`Backend::request_timeout`].
    pub request_timeout: Duration,
    /// Max distinct multi-turn `consult` sessions held in memory at once. Eviction
    /// is capacity-driven only (no TTL) — see [`crate::session`]. Server-wide (a
    /// session is a client thread, not a model trait).
    pub session_capacity: NonZeroUsize,
}

impl Default for Defaults {
    fn default() -> Self {
        // Mirror the historical consult defaults exactly (see the old
        // ConsultConfig::default + THINKING_BUDGET) so a config-less run is
        // byte-for-byte the prior behavior.
        Self {
            // High on purpose: a capable model rarely wastes turns, and hitting the
            // cap is no longer fatal — `run_phase` forces one final answer-now turn
            // from the partial transcript rather than discarding the work. So we'd
            // rather give the loop room (100 goes quickly in the explorer) than have
            // it bail early. The explorer sweeps breadth; the synth driver both
            // delegates sweeps and reads spans, so it gets the larger budget.
            explorer_max_turns: 100,
            synth_max_turns: 200,
            max_tokens: 16384,
            thinking_budget: crate::consult::THINKING_BUDGET,
            // Cold explorer for exact citations; a slightly warmer synth for the
            // answer prose. Low across the board — kaibo grounds, it doesn't riff.
            explorer_temperature: 0.1,
            synth_temperature: 0.3,
            top_p: 0.95,
            // "high" is valid on both effort-taking providers (Anthropic adaptive,
            // DeepSeek) and matches DeepSeek's prior hardcoded value, so a config-less
            // run is unchanged. `Auto` classifies the thinking style from the model id.
            explorer_effort: crate::consult::DEFAULT_EFFORT.to_string(),
            synth_effort: crate::consult::DEFAULT_EFFORT.to_string(),
            thinking_style: ThinkingStyleOverride::Auto,
            // 15 min. A single completion that takes longer is pathological for a
            // hosted API and a generous ceiling for a slow local model; either way
            // it bounds a wedged provider that would otherwise hang forever
            // (non-streaming loop → no other brake). Tune per backend if needed.
            request_timeout: Duration::from_secs(900),
            // 128 lean Q&A threads is a few KB of strings — generous for a personal
            // server, and capacity (not time) is the only eviction pressure.
            session_capacity: NonZeroUsize::new(128).expect("128 is nonzero"),
        }
    }
}

// --- Roles ------------------------------------------------------------------

/// The roles a cast's model slots can serve. Explorer and synth are the agent
/// phases; the media roles (the pal-merge production models — see `docs/issues.md`
/// "Media spine") are present only when configured: an absent slot means the
/// capability is absent, not an error (built-in casts always carry explorer+synth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelRole {
    Explorer,
    Synth,
    Image,
    Tts,
}

impl ModelRole {
    /// Every role, in table order — the source for "known roles" error text.
    pub const ALL: [ModelRole; 4] = [Self::Explorer, Self::Synth, Self::Image, Self::Tts];

    /// The role's config-table key (`[casts.<name>]`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Synth => "synth",
            Self::Image => "image",
            Self::Tts => "tts",
        }
    }
}

impl std::str::FromStr for ModelRole {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL.into_iter().find(|r| r.key() == s).ok_or_else(|| {
            let known: Vec<&str> = Self::ALL.iter().map(|r| r.key()).collect();
            anyhow!(
                "unknown model role {s:?}; known roles: {}",
                known.join(", ")
            )
        })
    }
}

// --- Slots ------------------------------------------------------------------

/// One entry in a cast's role table: which backend serves it, the model id, plus
/// per-slot capability pins and tunables. In TOML a slot is a `"backend/model-id"`
/// string (the common case) or a table (`{ backend = "…", id = "…", vision = true,
/// effort = "max", … }`) when a pin or tunable is needed. A slot ref borrows the
/// backend's *connection only* — it never follows another cast, so chains and
/// cycles are structurally impossible.
// Not `Eq`: `temperature` is `f64`. `PartialEq` is enough for the tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSlot {
    /// The backend serving this slot, stored as the *canonical* backend name
    /// (aliases like `claude` are resolved at load; a per-call qualified override
    /// resolves through [`Config::resolve_backend`]).
    pub backend: String,
    pub id: String,
    /// Vision-cap override for this slot; `None` asks the built-in classifier
    /// (see [`ModelCaps::resolve`]) against the slot's backend kind. The escape
    /// hatch for endpoints the classifier can't see into — a vision model behind
    /// a generic `openai` backend, say — exactly the `thinking_style` pattern.
    pub vision: Option<bool>,
    // -- per-slot tunables, each falling back to the [defaults] per-role value --
    pub max_tokens: Option<u64>,
    pub thinking_budget: Option<u64>,
    pub temperature: Option<f64>,
    pub effort: Option<String>,
    pub thinking_style: Option<ThinkingStyleOverride>,
}

impl ModelSlot {
    /// A slot with no pins and no tunables — the `"backend/id"` string form, and
    /// the shape a per-call model override produces (the override describes a new
    /// model, so the configured slot's pins don't carry).
    pub fn bare(backend: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            id: id.into(),
            vision: None,
            max_tokens: None,
            thinking_budget: None,
            temperature: None,
            effort: None,
            thinking_style: None,
        }
    }

    /// The slot rendered as its `"backend/id"` ref (the `kaibo://config` spelling).
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.backend, self.id)
    }

    /// Resolve this slot's effective tunables for `role`: the slot's own override
    /// wins, else the per-role `[defaults]` value. The single fallback point — the
    /// per-arm request shaping in `consult.rs` reads only the result.
    pub fn tunables(&self, role: ModelRole, defaults: &Defaults) -> SlotTunables {
        let (default_temperature, default_effort) = match role {
            ModelRole::Explorer => (
                defaults.explorer_temperature,
                defaults.explorer_effort.as_str(),
            ),
            // Synth and (future) media roles take the synth-side defaults: the
            // answer-composing posture is the general-purpose one.
            _ => (defaults.synth_temperature, defaults.synth_effort.as_str()),
        };
        SlotTunables {
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            thinking_budget: self.thinking_budget.unwrap_or(defaults.thinking_budget),
            temperature: self.temperature.unwrap_or(default_temperature),
            top_p: defaults.top_p,
            effort: self
                .effort
                .clone()
                .unwrap_or_else(|| default_effort.to_string()),
            thinking_style: self.thinking_style.unwrap_or(defaults.thinking_style),
        }
    }
}

/// A slot's effective request-shaping knobs after the per-role fallback (see
/// [`ModelSlot::tunables`]). What an arm actually sends.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotTunables {
    pub max_tokens: u64,
    pub thinking_budget: u64,
    pub temperature: f64,
    pub top_p: f64,
    pub effort: String,
    pub thinking_style: ThinkingStyleOverride,
}

/// Parse a `"backend/model-id"` slot ref. The *first* `/` splits — backend names
/// can't contain one, but model ids can (HuggingFace-style `org/model` ids behind
/// a local server keep their inner slash). Both halves must be non-empty.
pub fn parse_slot_ref(s: &str) -> Result<(String, String)> {
    let Some((backend, id)) = s.split_once('/') else {
        bail!("slot ref {s:?} must be \"backend/model-id\"");
    };
    let (backend, id) = (backend.trim(), id.trim());
    if backend.is_empty() || id.is_empty() {
        bail!("slot ref {s:?} must be \"backend/model-id\" with both parts non-empty");
    }
    Ok((backend.to_string(), id.to_string()))
}

// --- Backends ---------------------------------------------------------------

/// A connection: how kaibo reaches one provider endpoint. The `kind` is the wire
/// protocol (the closed [`ProviderKind`] enum — the one place "provider" still
/// means something); everything else describes the wire: endpoint, key source,
/// per-request deadline. Models live on [`Cast`] slots, never here.
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    /// The backend's name (what a cast slot's `backend` field references).
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
    /// Per-request deadline applied to this backend's HTTP client (`.timeout`):
    /// the wall-clock ceiling on a single completion call. rig's prompt loop is
    /// non-streaming and exposes no native timeout, so without this a provider
    /// that connects but never responds wedges the call indefinitely — exactly
    /// the 2026-06-06 stall (see `consult.rs` / `docs/issues.md`). Seeded from
    /// [`Defaults::request_timeout`], overridable per backend.
    pub request_timeout: Duration,
}

impl Backend {
    /// Resolve this backend's bearer token: configured env var (wins, when set and
    /// non-blank), then the key file, then — if `key_optional` — a placeholder, else
    /// a loud error.
    ///
    /// A *present but broken* key file (empty, unreadable, a directory) is a loud
    /// error even for a `key_optional` backend: a key that's there but wrong is a
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
                    .with_context(|| format!("resolving key for backend {:?}", self.name));
            }
        }

        // No env, no existing file.
        if self.key_optional {
            Ok(PLACEHOLDER_OPENAI_KEY.to_string())
        } else {
            Err(anyhow!(
                "backend {:?} has no API key: env {} unset and key file {} absent — \
                 set one, or key_optional = true only for a keyless endpoint",
                self.name,
                self.api_key_env.as_deref().unwrap_or("(none)"),
                self.api_key_file.as_deref().unwrap_or("(none)"),
            ))
        }
    }

    /// The base URL to dial for an OpenAI-compatible client. An explicit per-backend
    /// `base_url` wins; otherwise fall back to `OPENAI_BASE_URL` (back-compat) or the
    /// built-in default. Read at use-time, not construction, so backend *building*
    /// stays pure (see [`Config::from_toml_str`]).
    pub fn resolved_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(credentials::openai_base_url)
    }
}

// --- Casts ------------------------------------------------------------------

/// A composition: a named assignment of models to roles, freely spanning
/// backends. This is what the `cast` call param selects. A cast may omit roles —
/// the tool that needs a missing role fails loudly *at call time*, naming the gap
/// ("cast `tts-box` has no synth slot"); absent = capability absent.
// Not `Eq`: slots carry `f64` tunables. `PartialEq` is enough for the tests.
#[derive(Debug, Clone, PartialEq)]
pub struct Cast {
    /// The cast's name (the value the `cast` call arg carries).
    pub name: String,
    /// The role table: which slot serves each [`ModelRole`].
    pub slots: BTreeMap<ModelRole, ModelSlot>,
}

impl Cast {
    /// The slot serving `role`, if configured. `None` means the capability is
    /// absent; the *caller* decides whether that's an error for its tool.
    pub fn slot(&self, role: ModelRole) -> Option<&ModelSlot> {
        self.slots.get(&role)
    }

    /// The slot serving `role`, or the loud call-time error naming the gap.
    pub fn require_slot(&self, role: ModelRole) -> Result<&ModelSlot> {
        self.slot(role)
            .ok_or_else(|| anyhow!("cast {:?} has no {} slot", self.name, role.key()))
    }
}

// --- The whole config ------------------------------------------------------

/// kaibo's resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Default project root when a call omits `path` (`--root` / `KAIBO_ROOT`).
    pub root: Option<PathBuf>,
    /// Additional allowed path trees beyond `root`. A per-call `path` must
    /// canonicalize to at-or-under `root` OR one of these trees. Empty means
    /// "only root (or cwd fallback) is allowed". Set via `--allow-path`,
    /// `KAIBO_ALLOW_PATHS` (colon-separated), or `[server] allow_paths` in
    /// config.toml. Non-empty CLI list replaces lower layers.
    pub allow_paths: Vec<PathBuf>,
    /// Default cast name when a call omits `cast`.
    pub default_cast: String,
    /// `EnvFilter` directive used when `RUST_LOG` is unset.
    pub log: String,
    /// Which tools to advertise.
    pub tools: ToolGating,
    pub defaults: Defaults,
    /// Read-only sandbox limits (exec timeout, output cap, extra disabled builtins).
    pub sandbox: SandboxConfig,
    /// Backends (connections) by name.
    pub backends: BTreeMap<String, Backend>,
    /// Casts (compositions) by name.
    pub casts: BTreeMap<String, Cast>,
    /// alias → backend name (so slot refs like `claude/<id>` resolve).
    backend_aliases: BTreeMap<String, String>,
    /// alias → cast name (so `cast = "claude"` resolves).
    cast_aliases: BTreeMap<String, String>,
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
        Self::load_with(explicit_path, default_config_path(), |k| {
            std::env::var(k).ok()
        })
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
        // env'd defaults flow into the slots that inherit them.
        apply_raw_env(&mut raw, &get_env)?;
        Self::merge(raw)
    }

    /// Parse a config from a TOML string with **no** env or filesystem access — the
    /// pure entry point tests drive for merge precedence and validation.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(s).context("parsing config")?;
        Self::merge(raw)
    }

    /// Resolve a cast by name or alias. An unknown name is a loud error naming
    /// the available casts — the client asked for something we can't serve.
    pub fn resolve_cast(&self, name: &str) -> Result<&Cast> {
        if let Some(c) = self.casts.get(name) {
            return Ok(c);
        }
        if let Some(real) = self.cast_aliases.get(name) {
            return Ok(&self.casts[real]);
        }
        let names: Vec<&str> = self.casts.keys().map(String::as_str).collect();
        Err(anyhow!(
            "unknown cast {name:?}; known casts: {}",
            names.join(", ")
        ))
    }

    /// Resolve a backend by name or alias (the seam slot refs and per-call
    /// qualified overrides go through). An unknown name is a loud error naming
    /// the known backends.
    pub fn resolve_backend(&self, name: &str) -> Result<&Backend> {
        if let Some(b) = self.backends.get(name) {
            return Ok(b);
        }
        if let Some(real) = self.backend_aliases.get(name) {
            return Ok(&self.backends[real]);
        }
        let names: Vec<&str> = self.backends.keys().map(String::as_str).collect();
        Err(anyhow!(
            "unknown backend {name:?}; known backends: {}",
            names.join(", ")
        ))
    }

    /// alias → canonical backend name. Part of the resolved runtime state: an
    /// alias is a valid slot-ref prefix and per-call backend override, so the
    /// `kaibo://config` render exposes the registry for callers to discover.
    pub fn backend_aliases(&self) -> &BTreeMap<String, String> {
        &self.backend_aliases
    }

    /// alias → canonical cast name (each a valid `cast` call-param value).
    pub fn cast_aliases(&self) -> &BTreeMap<String, String> {
        &self.cast_aliases
    }

    /// The resolved capabilities of a slot's model: the slot's explicit pin wins,
    /// else the built-in classifier run against the *slot's backend kind*. This is
    /// the seam toolset assembly and the `kaibo://config` render read.
    pub fn slot_caps(&self, slot: &ModelSlot) -> Result<ModelCaps> {
        let backend = self.resolve_backend(&slot.backend)?;
        Ok(ModelCaps::resolve(backend.kind, &slot.id, slot.vision))
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
        // [profiles] is deleted, not deprecated: a config that still carries it
        // gets a loud pointer at the contract, never a silent reinterpretation.
        if raw.profiles.is_some() {
            bail!(
                "[profiles] no longer exists: the profile split into [backends.<name>] \
                 (connections) and [casts.<name>] (role → \"backend/model-id\") — \
                 see docs/casts.md"
            );
        }

        let defaults = merge_defaults(raw.defaults.unwrap_or_default())?;
        // Sampling knobs out of range are config typos, not intents — catch them at
        // load. Temperature spans providers (Anthropic 0–1, OpenAI/Gemini up to 2);
        // we accept the widest sane band and let the provider reject a value it
        // specifically dislikes. `top_p` is a probability in (0, 1].
        for (label, t) in [
            ("explorer_temperature", defaults.explorer_temperature),
            ("synth_temperature", defaults.synth_temperature),
        ] {
            if !(0.0..=2.0).contains(&t) {
                bail!("[defaults] {label} ({t}) must be in [0.0, 2.0]");
            }
        }
        if !(0.0 < defaults.top_p && defaults.top_p <= 1.0) {
            bail!(
                "[defaults] top_p ({}) must be in (0.0, 1.0]",
                defaults.top_p
            );
        }

        // --- Backends: built-ins, then the file's [backends] table. ---
        let mut backends = builtin_backends(&defaults);
        let mut backend_file_aliases: Vec<(String, String)> = Vec::new();
        for (name, rb) in raw.backends.unwrap_or_default() {
            match backends.get_mut(&name) {
                // Overriding a built-in (or earlier-defined) backend.
                Some(existing) => rb.apply_to(existing)?,
                // A new backend: seed from its kind's template, then apply.
                None => {
                    let kind = rb.kind.as_deref().ok_or_else(|| {
                        anyhow!("backend {name:?} is new and must declare a `kind`")
                    })?;
                    let kind: ProviderKind = kind
                        .parse()
                        .with_context(|| format!("backend {name:?} kind"))?;
                    let mut b = backend_template(kind, &defaults);
                    b.name = name.clone();
                    rb.apply_to(&mut b)?;
                    // A NEW openai-kind backend must say where it points: the
                    // use-time fallback (OPENAI_BASE_URL, else the local default)
                    // belongs to the built-in `openai` backend alone — inherited
                    // here it would silently dial the wrong server and fail as a
                    // cryptic 404 mid-call instead of loudly at load.
                    if b.kind == ProviderKind::Openai && b.base_url.is_none() {
                        bail!(
                            "backend {name:?} (kind \"openai\") must set base_url — \
                             only the built-in `openai` backend falls back to \
                             OPENAI_BASE_URL / the local default"
                        );
                    }
                    backends.insert(name.clone(), b);
                }
            }
            for alias in rb.aliases.into_iter().flatten() {
                backend_file_aliases.push((alias, name.clone()));
            }
        }

        // Validate backends. These are config mistakes, not no-ops: crash loudly
        // at startup rather than fail cryptically mid-call.
        for b in backends.values() {
            // A base_url on a keyed kind: rig fixes those endpoints.
            if b.kind != ProviderKind::Openai && b.base_url.is_some() {
                bail!(
                    "backend {:?} (kind {:?}) sets base_url, but only the `openai` kind \
                     has a configurable endpoint",
                    b.name,
                    b.kind
                );
            }
            // A zero deadline means "time out instantly" — never a real intent, and
            // it would brick every call. Catch it at load, not as a mystery failure
            // on the first request. (There is no "disable" escape hatch: an infinite
            // wait is the bug this field exists to prevent.)
            if b.request_timeout.is_zero() {
                bail!(
                    "backend {:?}: request_timeout_secs must be > 0 — a zero deadline \
                     times out every call instantly",
                    b.name,
                );
            }
        }

        // Backend aliases: built-ins first, then file-declared. Collisions are loud,
        // per level.
        let mut backend_aliases: BTreeMap<String, String> = BTreeMap::new();
        for name in backends.keys() {
            for alias in builtin_aliases(name) {
                register_alias("backend", &mut backend_aliases, &backends, alias, name)?;
            }
        }
        for (alias, name) in backend_file_aliases {
            register_alias("backend", &mut backend_aliases, &backends, alias, &name)?;
        }

        // --- Casts: built-ins, then the file's [casts] table (role-wise merge). ---
        let mut casts = builtin_casts();
        let mut cast_file_aliases: Vec<(String, String)> = Vec::new();
        for (name, rc) in raw.casts.unwrap_or_default() {
            let cast = casts.entry(name.clone()).or_insert_with(|| Cast {
                name: name.clone(),
                slots: BTreeMap::new(),
            });
            for (role, raw_slot) in rc.slots() {
                let slot = raw_slot
                    .clone()
                    .into_slot()
                    .with_context(|| format!("cast {name:?} {} slot", role.key()))?;
                cast.slots.insert(role, slot);
            }
            for alias in rc.aliases.into_iter().flatten() {
                cast_file_aliases.push((alias, name.clone()));
            }
        }

        // Resolve every slot's backend ref through the alias map (stored canonical),
        // and validate the slot. Unknown backend → loud error naming the known set.
        for cast in casts.values_mut() {
            let Cast { name, slots } = cast;
            for (role, slot) in slots.iter_mut() {
                if !backends.contains_key(&slot.backend) {
                    match backend_aliases.get(&slot.backend) {
                        Some(real) => slot.backend = real.clone(),
                        None => {
                            let known: Vec<&str> = backends.keys().map(String::as_str).collect();
                            bail!(
                                "cast {name:?}: the {} slot names unknown backend {:?}; \
                                 known backends: {}",
                                role.key(),
                                slot.backend,
                                known.join(", ")
                            );
                        }
                    }
                }
                // An empty model id in any slot is a typo, never an intent — it would
                // surface as a baffling provider 404 mid-call otherwise.
                if slot.id.trim().is_empty() {
                    bail!("cast {name:?}: the {} model id is empty", role.key());
                }
                if let Some(t) = slot.temperature {
                    if !(0.0..=2.0).contains(&t) {
                        bail!(
                            "cast {name:?} {} slot: temperature ({t}) must be in [0.0, 2.0]",
                            role.key()
                        );
                    }
                }
                // Thinking-on kinds need output headroom above the reasoning budget;
                // Anthropic *requires* max_tokens > budget_tokens (see consult.rs).
                // Validate the slot's *resolved* values (per-slot override else
                // defaults) so an inverted pair is caught here, not as a 400 mid-call.
                let kind = backends[&slot.backend].kind;
                let t = slot.tunables(*role, &defaults);
                if matches!(kind, ProviderKind::Anthropic | ProviderKind::Gemini)
                    && t.thinking_budget >= t.max_tokens
                {
                    bail!(
                        "cast {name:?} {} slot: thinking_budget ({}) must be < \
                         max_tokens ({}) — reasoning would starve the answer \
                         (Anthropic rejects it outright)",
                        role.key(),
                        t.thinking_budget,
                        t.max_tokens
                    );
                }
            }
        }

        // Cast aliases, same discipline as backend aliases — the built-in names
        // (`claude`, `google`, `local`, …) register at BOTH levels so a cast ref
        // and a slot ref both resolve.
        let mut cast_aliases: BTreeMap<String, String> = BTreeMap::new();
        for name in casts.keys() {
            for alias in builtin_aliases(name) {
                register_alias("cast", &mut cast_aliases, &casts, alias, name)?;
            }
        }
        for (alias, name) in cast_file_aliases {
            register_alias("cast", &mut cast_aliases, &casts, alias, &name)?;
        }

        let server = raw.server.unwrap_or_default();
        let tools = merge_tools(server.tools.unwrap_or_default());
        let default_cast = server.cast.unwrap_or_else(|| "anthropic".to_string());
        let log = server.log.unwrap_or_else(|| "kaibo=info".to_string());
        let root = server.root.map(PathBuf::from);
        let allow_paths = server
            .allow_paths
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let sandbox = merge_sandbox(raw.sandbox.unwrap_or_default());

        let cfg = Self {
            root,
            allow_paths,
            default_cast,
            log,
            tools,
            defaults,
            sandbox,
            backends,
            casts,
            backend_aliases,
            cast_aliases,
        };

        // default_cast must resolve now (CLI may still override it; main
        // re-validates the final choice). Catch a typo in the file early.
        cfg.resolve_cast(&cfg.default_cast).with_context(|| {
            format!(
                "config `server.cast` = {:?} names no cast",
                cfg.default_cast
            )
        })?;
        Ok(cfg)
    }

    /// Apply CLI overrides (highest precedence). Called from `main` after load.
    /// CLI can only *disable* a tool (the `--no-<tool>` flags); enabling is the job
    /// of the file/env/built-in layers. A non-empty `allow_paths` replaces lower layers.
    pub fn apply_cli(
        &mut self,
        root: Option<PathBuf>,
        cast: Option<String>,
        disable: ToolDisables,
        allow_paths: Vec<PathBuf>,
    ) {
        if let Some(root) = root {
            self.root = Some(root);
        }
        if let Some(cast) = cast {
            self.default_cast = cast;
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
        // Non-empty CLI allow_paths replaces lower layers (env/file).
        if !allow_paths.is_empty() {
            self.allow_paths = allow_paths;
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

/// Register `alias → target` at one level (backend or cast), rejecting a clash
/// with a real name at that level or a different existing alias target.
fn register_alias<T>(
    level: &str,
    aliases: &mut BTreeMap<String, String>,
    names: &BTreeMap<String, T>,
    alias: String,
    target: &str,
) -> Result<()> {
    if names.contains_key(&alias) {
        bail!("{level} alias {alias:?} (for {target:?}) collides with a {level} of the same name");
    }
    if let Some(prev) = aliases.get(&alias) {
        if prev != target {
            bail!("{level} alias {alias:?} is claimed by both {prev:?} and {target:?}");
        }
    }
    aliases.insert(alias, target.to_string());
    Ok(())
}

// --- Built-in registry -----------------------------------------------------

/// The built-in aliases for a built-in name. These register at BOTH levels —
/// as backend aliases (so a slot ref like `claude/<id>` resolves) and as cast
/// aliases (so `cast = "claude"` resolves). Empty for user-defined names —
/// file-declared aliases ride along on the raw stanzas and merge separately.
fn builtin_aliases(name: &str) -> Vec<String> {
    let v: &[&str] = match name {
        "anthropic" => &["claude"],
        "gemini" => &["google"],
        "openai" => &["local", "lemonade", "gemma", "gemma4"],
        _ => &[],
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// A fresh backend template for `kind`, carrying that kind's default key source.
/// New file backends start here, then apply their overrides.
fn backend_template(kind: ProviderKind, defaults: &Defaults) -> Backend {
    Backend {
        name: String::new(),
        kind,
        // No base_url baked in: `resolved_base_url` supplies the default (or
        // OPENAI_BASE_URL, for the openai kind) at use-time, keeping construction
        // pure. A keyed kind with an explicit base_url is rejected in `merge`.
        base_url: None,
        api_key_env: Some(kind.env_var().to_string()),
        api_key_file: Some(format!("~/{}", kind.key_file_name())),
        key_optional: kind.key_optional(),
        request_timeout: defaults.request_timeout,
    }
}

/// The four built-in backends, named after their kind.
fn builtin_backends(defaults: &Defaults) -> BTreeMap<String, Backend> {
    let mut m = BTreeMap::new();
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::Openai,
    ] {
        let mut b = backend_template(kind, defaults);
        b.name = kind.canonical_name().to_string();
        m.insert(b.name.clone(), b);
    }
    m
}

/// The four built-in casts: same-named single-backend compositions carrying
/// today's default models, so a missing config file and `cast = "anthropic"`
/// reproduce kaibo's historical behavior byte-for-byte.
fn builtin_casts() -> BTreeMap<String, Cast> {
    let mut m = BTreeMap::new();
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::Openai,
    ] {
        let name = kind.canonical_name().to_string();
        let (explorer, synth) = default_models(kind);
        // Only the agent roles are seeded: a media role (image, tts) appears in
        // the table only when a config asks for it — absent means the capability
        // is absent, and nothing downstream errors on that.
        let slots = BTreeMap::from([
            (ModelRole::Explorer, ModelSlot::bare(&name, explorer)),
            (ModelRole::Synth, ModelSlot::bare(&name, synth)),
        ]);
        m.insert(name.clone(), Cast { name, slots });
    }
    m
}

/// Default (explorer, synth) model ids per kind. The seed values for the built-in
/// casts; they drift — keep in sync with the source-of-truth pal configs
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
    backends: Option<BTreeMap<String, RawBackend>>,
    casts: Option<BTreeMap<String, RawCast>>,
    /// Tombstone. `[profiles]` was split into `[backends]` + `[casts]`
    /// (docs/casts.md); its presence — any shape — is a load error in `merge`.
    /// Declared so the message points at the migration instead of serde's
    /// generic unknown-field complaint.
    profiles: Option<toml::Value>,
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
    /// Additional allowed path trees: a per-call `path` must canonicalize to
    /// at-or-under `root` OR one of these. Env override: `KAIBO_ALLOW_PATHS`
    /// (colon-separated). CLI override: repeatable `--allow-path DIR`.
    allow_paths: Option<Vec<String>>,
    /// The default cast (was `provider` before the backends/casts split; the old
    /// key is now an unknown-field load error via `deny_unknown_fields`).
    cast: Option<String>,
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
    explorer_temperature: Option<f64>,
    synth_temperature: Option<f64>,
    top_p: Option<f64>,
    explorer_effort: Option<String>,
    synth_effort: Option<String>,
    thinking_style: Option<String>,
    request_timeout_secs: Option<u64>,
    session_capacity: Option<usize>,
}

/// One `[backends.<name>]` stanza: connection knobs only — models live on casts.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    kind: Option<String>,
    aliases: Option<Vec<String>>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    key_optional: Option<bool>,
    request_timeout_secs: Option<u64>,
}

impl RawBackend {
    /// Overlay this raw backend's set fields onto a resolved [`Backend`]. A `kind`
    /// that disagrees with the target's existing kind is a loud error (you don't
    /// change a backend's protocol by re-listing it).
    fn apply_to(&self, b: &mut Backend) -> Result<()> {
        if let Some(kind) = &self.kind {
            let kind: ProviderKind = kind
                .parse()
                .with_context(|| format!("backend {:?} kind", b.name))?;
            if kind != b.kind {
                bail!(
                    "backend {:?} declares kind {:?} but already exists as kind {:?}",
                    b.name,
                    kind,
                    b.kind
                );
            }
        }
        if let Some(v) = &self.base_url {
            b.base_url = Some(v.clone());
        }
        if let Some(v) = &self.api_key_env {
            b.api_key_env = Some(v.clone());
        }
        if let Some(v) = &self.api_key_file {
            b.api_key_file = Some(v.clone());
        }
        if let Some(v) = self.key_optional {
            b.key_optional = v;
        }
        if let Some(v) = self.request_timeout_secs {
            b.request_timeout = Duration::from_secs(v);
        }
        Ok(())
    }
}

/// One `[casts.<name>]` stanza: a slot per role, each a `"backend/model-id"`
/// string or a table with pins/tunables. The role keys are struct fields (not a
/// free map) so `deny_unknown_fields` makes a typo'd role a loud load error.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCast {
    aliases: Option<Vec<String>>,
    explorer: Option<RawSlot>,
    synth: Option<RawSlot>,
    image: Option<RawSlot>,
    tts: Option<RawSlot>,
}

impl RawCast {
    /// The configured (role, slot) pairs, in role order.
    fn slots(&self) -> Vec<(ModelRole, &RawSlot)> {
        [
            (ModelRole::Explorer, &self.explorer),
            (ModelRole::Synth, &self.synth),
            (ModelRole::Image, &self.image),
            (ModelRole::Tts, &self.tts),
        ]
        .into_iter()
        .filter_map(|(role, slot)| slot.as_ref().map(|s| (role, s)))
        .collect()
    }
}

/// One slot as written: a `"backend/model-id"` ref, or a table when the slot
/// carries pins or tunables. Deserialized by hand rather than `#[serde(untagged)]`:
/// untagged dispatch discards each variant's own error, reducing a typo'd tunable
/// to "data did not match any variant" — naming neither the bad key nor the fix.
/// Forking on the TOML value's shape lets [`RawSlotTable`]'s deny_unknown_fields
/// error (which names the unknown field and the valid knobs) reach the user.
#[derive(Debug, Clone)]
enum RawSlot {
    Ref(String),
    Table(RawSlotTable),
}

impl<'de> Deserialize<'de> for RawSlot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        match toml::Value::deserialize(deserializer)? {
            toml::Value::String(s) => Ok(Self::Ref(s)),
            v @ toml::Value::Table(_) => RawSlotTable::deserialize(v)
                .map(Self::Table)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "a slot is a \"backend/model-id\" string or a table \
                 {{ backend, id, … }}, got a {}",
                other.type_str()
            ))),
        }
    }
}

/// The table form of a slot. A separate struct (not inline in the enum) so
/// `deny_unknown_fields` applies — a typo'd tunable must not silently vanish.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlotTable {
    backend: String,
    id: String,
    vision: Option<bool>,
    max_tokens: Option<u64>,
    thinking_budget: Option<u64>,
    temperature: Option<f64>,
    effort: Option<String>,
    thinking_style: Option<String>,
}

impl RawSlot {
    fn into_slot(self) -> Result<ModelSlot> {
        match self {
            Self::Ref(s) => {
                let (backend, id) = parse_slot_ref(&s)?;
                Ok(ModelSlot::bare(backend, id))
            }
            Self::Table(t) => Ok(ModelSlot {
                backend: t.backend,
                id: t.id,
                vision: t.vision,
                max_tokens: t.max_tokens,
                thinking_budget: t.thinking_budget,
                temperature: t.temperature,
                effort: t.effort,
                thinking_style: t
                    .thinking_style
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .context("thinking_style")?,
            }),
        }
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
        explorer_temperature: raw.explorer_temperature.unwrap_or(d.explorer_temperature),
        synth_temperature: raw.synth_temperature.unwrap_or(d.synth_temperature),
        top_p: raw.top_p.unwrap_or(d.top_p),
        explorer_effort: raw.explorer_effort.unwrap_or(d.explorer_effort),
        synth_effort: raw.synth_effort.unwrap_or(d.synth_effort),
        thinking_style: match raw.thinking_style {
            Some(s) => s.parse().context("[defaults] thinking_style")?,
            None => d.thinking_style,
        },
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
    // Tombstone: the old selector env var must not be silently ignored into the
    // default cast — name its replacement and stop.
    if get("KAIBO_PROVIDER").is_some() {
        bail!(
            "KAIBO_PROVIDER is gone: the profile split into backends and casts — \
             set KAIBO_CAST instead (see docs/casts.md)"
        );
    }
    let server = raw.server.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_ROOT") {
        server.root = Some(v);
    }
    if let Some(v) = get("KAIBO_ALLOW_PATHS") {
        // Colon-separated like PATH; an empty component is silently skipped.
        let paths: Vec<String> = std::env::split_paths(&v)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if !paths.is_empty() {
            server.allow_paths = Some(paths);
        }
    }
    if let Some(v) = get("KAIBO_CAST") {
        server.cast = Some(v);
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
        defaults.explorer_max_turns = Some(parse_env_int("KAIBO_EXPLORER_MAX_TURNS", &v)?);
    }
    if let Some(v) = get("KAIBO_SYNTH_MAX_TURNS") {
        defaults.synth_max_turns = Some(parse_env_int("KAIBO_SYNTH_MAX_TURNS", &v)?);
    }
    if let Some(v) = get("KAIBO_MAX_TOKENS") {
        defaults.max_tokens = Some(parse_env_int("KAIBO_MAX_TOKENS", &v)?);
    }
    if let Some(v) = get("KAIBO_THINKING_BUDGET") {
        defaults.thinking_budget = Some(parse_env_int("KAIBO_THINKING_BUDGET", &v)?);
    }
    if let Some(v) = get("KAIBO_EXPLORER_TEMPERATURE") {
        defaults.explorer_temperature = Some(parse_env("KAIBO_EXPLORER_TEMPERATURE", &v)?);
    }
    if let Some(v) = get("KAIBO_SYNTH_TEMPERATURE") {
        defaults.synth_temperature = Some(parse_env("KAIBO_SYNTH_TEMPERATURE", &v)?);
    }
    if let Some(v) = get("KAIBO_TOP_P") {
        defaults.top_p = Some(parse_env("KAIBO_TOP_P", &v)?);
    }
    // Effort and thinking_style are strings (provider-validated / parsed at merge),
    // so no numeric parse here — just fold the raw value in.
    if let Some(v) = get("KAIBO_EXPLORER_EFFORT") {
        defaults.explorer_effort = Some(v);
    }
    if let Some(v) = get("KAIBO_SYNTH_EFFORT") {
        defaults.synth_effort = Some(v);
    }
    if let Some(v) = get("KAIBO_THINKING_STYLE") {
        defaults.thinking_style = Some(v);
    }
    if let Some(v) = get("KAIBO_REQUEST_TIMEOUT_SECS") {
        defaults.request_timeout_secs = Some(parse_env_int("KAIBO_REQUEST_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_SESSION_CAPACITY") {
        defaults.session_capacity = Some(parse_env_int("KAIBO_SESSION_CAPACITY", &v)?);
    }

    let sandbox = raw.sandbox.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_EXEC_TIMEOUT_SECS") {
        sandbox.exec_timeout_secs = Some(parse_env_int("KAIBO_EXEC_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_OUTPUT_LIMIT_BYTES") {
        sandbox.output_limit_bytes = Some(parse_env_int("KAIBO_OUTPUT_LIMIT_BYTES", &v)?);
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

/// Parse an integer env tunable, bounded at `i64::MAX`. TOML integers are i64,
/// so the config-*file* path structurally can't carry a larger value — only env
/// can, and a quintillion-token budget is never an intent (same spirit as the
/// zero-timeout check). Unbounded, it would also panic the first `kaibo://config`
/// read: the render serializes the resolved value back to TOML, and the TOML
/// serializer rejects a u64 above i64::MAX. Loud at load instead.
fn parse_env_int<T: TryFrom<u64>>(name: &str, v: &str) -> Result<T> {
    let n: u64 = parse_env(name, v)?;
    if n > i64::MAX as u64 {
        bail!("{name}={v:?} is too large (max {})", i64::MAX);
    }
    T::try_from(n).map_err(|_| anyhow!("{name}={v:?} is out of range"))
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
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("kaibo")
            .join("config.toml")
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- built-in equivalence -------------------------------------------------

    /// A missing config file is a non-error and `cast = "anthropic"` reproduces
    /// today's behavior: the same default models on the same-named backend.
    #[test]
    fn builtin_cast_anthropic_carries_todays_models() {
        let cfg = Config::builtin();
        assert_eq!(cfg.default_cast, "anthropic");
        let cast = cfg.resolve_cast("anthropic").unwrap();
        let (explorer, synth) = default_models(ProviderKind::Anthropic);
        let e = cast.require_slot(ModelRole::Explorer).unwrap();
        let s = cast.require_slot(ModelRole::Synth).unwrap();
        assert_eq!((e.backend.as_str(), e.id.as_str()), ("anthropic", explorer));
        assert_eq!((s.backend.as_str(), s.id.as_str()), ("anthropic", synth));
        // Connection details ride the backend, with today's key sources.
        let b = cfg.resolve_backend("anthropic").unwrap();
        assert_eq!(b.kind, ProviderKind::Anthropic);
        assert_eq!(b.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(!b.key_optional);
    }

    /// All four built-in casts exist, each single-backend with explorer+synth.
    #[test]
    fn all_four_builtin_casts_resolve() {
        let cfg = Config::builtin();
        for name in ["anthropic", "deepseek", "gemini", "openai"] {
            let cast = cfg.resolve_cast(name).unwrap();
            for role in [ModelRole::Explorer, ModelRole::Synth] {
                let slot = cast.require_slot(role).unwrap();
                assert_eq!(slot.backend, name, "built-in casts are single-backend");
            }
        }
    }

    /// The built-in aliases register at BOTH levels: `claude` resolves as a cast
    /// name AND as a backend ref inside a slot.
    #[test]
    fn builtin_aliases_register_at_both_levels() {
        let cfg = Config::builtin();
        // Cast level.
        assert_eq!(cfg.resolve_cast("claude").unwrap().name, "anthropic");
        assert_eq!(cfg.resolve_cast("google").unwrap().name, "gemini");
        for a in ["local", "lemonade", "gemma", "gemma4"] {
            assert_eq!(cfg.resolve_cast(a).unwrap().name, "openai");
        }
        // Backend level.
        assert_eq!(cfg.resolve_backend("claude").unwrap().name, "anthropic");
        assert_eq!(cfg.resolve_backend("google").unwrap().name, "gemini");
        assert_eq!(cfg.resolve_backend("local").unwrap().name, "openai");
        // And a slot ref written against an alias canonicalizes at load.
        let cfg = Config::from_toml_str(
            r#"
            [casts.x]
            synth = "claude/claude-sonnet-4-6"
            "#,
        )
        .unwrap();
        let slot = cfg
            .resolve_cast("x")
            .unwrap()
            .require_slot(ModelRole::Synth)
            .unwrap();
        assert_eq!(slot.backend, "anthropic", "alias ref stored canonical");
    }

    // --- chimera parse ----------------------------------------------------------

    /// The whole point: a cast freely spanning backends, written with both slot
    /// forms, each slot's caps classified on ITS OWN backend's kind.
    #[test]
    fn a_chimera_cast_spans_backends_with_both_slot_forms() {
        let cfg = Config::from_toml_str(
            r#"
            [backends.sd]
            kind = "openai"
            base_url = "http://localhost:7860/v1"
            key_optional = true

            [casts.chimera]
            explorer = "deepseek/deepseek-v4-flash"
            synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
            image = "sd/sdxl-turbo"
            "#,
        )
        .unwrap();
        let cast = cfg.resolve_cast("chimera").unwrap();
        let e = cast.require_slot(ModelRole::Explorer).unwrap();
        let s = cast.require_slot(ModelRole::Synth).unwrap();
        let i = cast.require_slot(ModelRole::Image).unwrap();
        assert_eq!(e.qualified(), "deepseek/deepseek-v4-flash");
        assert_eq!(s.qualified(), "anthropic/claude-opus-4-8");
        assert_eq!(s.effort.as_deref(), Some("max"));
        assert_eq!(i.qualified(), "sd/sdxl-turbo");
        // No synth-less surprise: an omitted role is absent, not defaulted.
        assert!(cast.slot(ModelRole::Tts).is_none());
        // Caps classify on the slot's backend kind: DeepSeek is blind, Anthropic sees.
        assert!(!cfg.slot_caps(e).unwrap().vision);
        assert!(cfg.slot_caps(s).unwrap().vision);
        // A vision pin on a generic openai slot overrides the classifier.
        let pinned = ModelSlot {
            vision: Some(true),
            ..ModelSlot::bare("sd", "llava")
        };
        assert!(cfg.slot_caps(&pinned).unwrap().vision);
    }

    /// A model id with an inner slash (HuggingFace style) survives the string
    /// form: only the FIRST `/` splits backend from id.
    #[test]
    fn slot_ref_splits_on_the_first_slash_only() {
        let (backend, id) = parse_slot_ref("openai/Qwen/Qwen3-32B").unwrap();
        assert_eq!(backend, "openai");
        assert_eq!(id, "Qwen/Qwen3-32B");
        assert!(parse_slot_ref("no-slash-here").is_err());
        assert!(parse_slot_ref("/id-only").is_err());
        assert!(parse_slot_ref("backend/").is_err());
    }

    /// Overriding a built-in cast replaces only the roles the file sets.
    #[test]
    fn a_file_cast_stanza_merges_role_wise_over_a_builtin() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.anthropic]
            synth = "anthropic/claude-opus-4-8"
            "#,
        )
        .unwrap();
        let cast = cfg.resolve_cast("anthropic").unwrap();
        assert_eq!(
            cast.require_slot(ModelRole::Synth).unwrap().id,
            "claude-opus-4-8"
        );
        // The explorer slot is untouched (still the built-in default).
        assert_eq!(
            cast.require_slot(ModelRole::Explorer).unwrap().id,
            default_models(ProviderKind::Anthropic).0
        );
    }

    // --- per-slot tunables ------------------------------------------------------

    /// Per-slot tunables win; everything unset falls back to the per-role default.
    #[test]
    fn slot_tunables_fall_back_to_per_role_defaults() {
        let defaults = Defaults::default();
        let bare = ModelSlot::bare("anthropic", "m");
        let t = bare.tunables(ModelRole::Explorer, &defaults);
        assert_eq!(t.max_tokens, defaults.max_tokens);
        assert_eq!(t.thinking_budget, defaults.thinking_budget);
        assert_eq!(t.temperature, defaults.explorer_temperature);
        assert_eq!(t.effort, defaults.explorer_effort);
        assert_eq!(t.thinking_style, defaults.thinking_style);
        // Synth role picks the synth-side defaults.
        let t = bare.tunables(ModelRole::Synth, &defaults);
        assert_eq!(t.temperature, defaults.synth_temperature);
        assert_eq!(t.effort, defaults.synth_effort);
        // Per-slot overrides win.
        let tuned = ModelSlot {
            max_tokens: Some(32768),
            thinking_budget: Some(1024),
            temperature: Some(0.7),
            effort: Some("max".into()),
            thinking_style: Some(ThinkingStyleOverride::Adaptive),
            ..ModelSlot::bare("anthropic", "m")
        };
        let t = tuned.tunables(ModelRole::Synth, &defaults);
        assert_eq!(
            (t.max_tokens, t.thinking_budget, t.temperature),
            (32768, 1024, 0.7)
        );
        assert_eq!(t.effort, "max");
        assert_eq!(t.thinking_style, ThinkingStyleOverride::Adaptive);
    }

    // --- loud errors --------------------------------------------------------------

    /// An unknown backend in a slot is a load error naming the known backends.
    #[test]
    fn unknown_backend_in_a_slot_is_a_load_error_naming_the_known_set() {
        let err = Config::from_toml_str(
            r#"
            [casts.x]
            synth = "nope/some-model"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown backend \"nope\""), "got: {err}");
        assert!(err.contains("known backends"), "got: {err}");
        assert!(err.contains("anthropic"), "got: {err}");
    }

    /// `[profiles]` is deleted, not deprecated: a leftover table is a load error
    /// pointing at the migration doc, whatever its shape.
    #[test]
    fn a_profiles_table_is_a_load_error_pointing_at_the_contract() {
        let err = Config::from_toml_str(
            r#"
            [profiles.anthropic]
            synth_model = "claude-opus-4-8"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[profiles]"), "got: {err}");
        assert!(err.contains("docs/casts.md"), "got: {err}");
        assert!(err.contains("[backends"), "got: {err}");
        assert!(err.contains("[casts"), "got: {err}");
    }

    /// KAIBO_PROVIDER set is a load error naming KAIBO_CAST — never a silent
    /// fall-through to the default cast.
    #[test]
    fn env_kaibo_provider_is_a_load_error_naming_kaibo_cast() {
        let err = Config::load_with(None, None, |k| {
            (k == "KAIBO_PROVIDER").then(|| "anthropic".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("KAIBO_PROVIDER"), "got: {err}");
        assert!(err.contains("KAIBO_CAST"), "got: {err}");
    }

    /// KAIBO_CAST is the live env selector for the default cast.
    #[test]
    fn env_kaibo_cast_selects_the_default_cast() {
        let cfg = Config::load_with(None, None, |k| {
            (k == "KAIBO_CAST").then(|| "gemini".to_string())
        })
        .unwrap();
        assert_eq!(cfg.default_cast, "gemini");
    }

    /// `[server] cast` selects the default; an unresolvable name is loud.
    #[test]
    fn server_cast_selects_the_default_and_validates() {
        let cfg = Config::from_toml_str("[server]\ncast = \"deepseek\"\n").unwrap();
        assert_eq!(cfg.default_cast, "deepseek");
        let err = Config::from_toml_str("[server]\ncast = \"nope\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("server.cast"), "got: {err}");
        // The old key is an unknown-field load error (deny_unknown_fields).
        assert!(Config::from_toml_str("[server]\nprovider = \"anthropic\"\n").is_err());
    }

    /// An empty model id is a load error — it would 404 cryptically mid-call.
    #[test]
    fn an_empty_model_id_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.x]
            synth = { backend = "anthropic", id = " " }
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("model id is empty"), "got: {err}");
    }

    /// A new backend must declare a kind; redeclaring a different kind on an
    /// existing one is a loud error; base_url is openai-kind only.
    #[test]
    fn backend_stanza_validation_is_loud() {
        let err = Config::from_toml_str("[backends.mine]\nbase_url = \"http://x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must declare a `kind`"), "got: {err}");

        let err = Config::from_toml_str("[backends.anthropic]\nkind = \"gemini\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists as kind"), "got: {err}");

        let err = Config::from_toml_str("[backends.anthropic]\nbase_url = \"http://x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only the `openai` kind"), "got: {err}");
    }

    /// Alias collisions are loud, per level: a user cast named like a built-in
    /// alias collides with that alias.
    #[test]
    fn alias_collisions_are_loud_per_level() {
        // A cast named "claude" collides with the built-in cast alias claude→anthropic.
        let err = Config::from_toml_str(
            r#"
            [casts.claude]
            synth = "anthropic/claude-opus-4-8"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cast alias \"claude\""), "got: {err}");
        assert!(err.contains("collides"), "got: {err}");

        // A backend named "google" collides with the built-in backend alias.
        // (base_url set so the new-openai-backend rule doesn't fire first — the
        // collision is what's under test.)
        let err = Config::from_toml_str(
            "[backends.google]\nkind = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("backend alias \"google\""), "got: {err}");

        // Two casts claiming the same file alias collide.
        let err = Config::from_toml_str(
            r#"
            [casts.a]
            aliases = ["fast"]
            synth = "anthropic/claude-sonnet-4-6"
            [casts.b]
            aliases = ["fast"]
            synth = "deepseek/deepseek-v4-pro"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("claimed by both"), "got: {err}");
    }

    /// A slot whose resolved thinking budget would starve its resolved max_tokens
    /// on a thinking-on kind is caught at load, not as a 400 mid-call.
    #[test]
    fn an_inverted_thinking_budget_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.x]
            synth = { backend = "anthropic", id = "claude-sonnet-4-6", max_tokens = 1000, thinking_budget = 2000 }
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("thinking_budget"), "got: {err}");
        assert!(err.contains("max_tokens"), "got: {err}");
        // The same pair on a non-thinking-budget kind is accepted.
        Config::from_toml_str(
            r#"
            [casts.x]
            synth = { backend = "openai", id = "m", max_tokens = 1000, thinking_budget = 2000 }
            "#,
        )
        .unwrap();
    }

    /// An unknown cast at resolve time names the known casts.
    #[test]
    fn unknown_cast_is_a_loud_error_naming_known_casts() {
        let cfg = Config::builtin();
        let err = cfg.resolve_cast("nope").unwrap_err().to_string();
        assert!(err.contains("unknown cast"), "got: {err}");
        assert!(err.contains("known casts"), "got: {err}");
        assert!(err.contains("anthropic"), "got: {err}");
    }

    /// A typo'd role key in a cast stanza is a loud load error (deny_unknown_fields).
    #[test]
    fn an_unknown_role_key_in_a_cast_is_a_load_error() {
        assert!(Config::from_toml_str(
            r#"
            [casts.x]
            explorrer = "anthropic/claude-haiku-4-5"
            "#,
        )
        .is_err());
    }
}
