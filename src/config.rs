//! kaibo's configuration: connections, compositions, and tunable defaults,
//! layered CLI > env > file > built-in.
//!
//! The load-bearing idea is the split between a [`Backend`] (a *connection*: a
//! [`ProviderKind`] wire protocol plus base URL, key source, and request timeout)
//! and a [`Cast`] (a *composition*: a named assignment of models to
//! [`ModelRole`]s, freely spanning backends). Calls pick casts; backends are
//! reachable only through a cast's slots. That indirection is what lets "a cheap
//! local deepseek explorer feeding a claude synth" be one named thing — the old fused
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
use serde::{Deserialize, Serialize};

use crate::consult::{ModelCaps, PromptOverrides, ThinkingStyleOverride};
use crate::context::ContextConfig;
use crate::credentials::{self, ProviderKind, PLACEHOLDER_OPENAI_KEY};
use crate::orientation::OrientationConfig;
use crate::sandbox::SandboxConfig;
use crate::server::ToolGating;
use kaish_kernel::{IgnoreConfig, IgnoreScope};

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
    /// Max async-`consult` jobs (`consult_submit`) held in memory at once — running
    /// plus finished-but-uncollected. Capacity-LRU like sessions, no TTL; evicting a
    /// still-running job aborts it (see [`crate::jobs`]). Its own knob because a job
    /// result (a full answer + optional explorer report) is heavier than a session's
    /// lean Q&A pair, so the honest cap is smaller.
    pub job_capacity: NonZeroUsize,
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
            // Fewer than sessions: a held job result (answer + maybe a report) is
            // heavier, and a caller rarely has many async consults in flight at once.
            // 64 is generous headroom before the LRU starts aborting the oldest
            // still-running job to make room.
            job_capacity: NonZeroUsize::new(64).expect("64 is nonzero"),
        }
    }
}

// --- Roles ------------------------------------------------------------------

/// The roles a cast's model slots can serve: the two agent phases. Explorer does the
/// fast sweeps; synth answers. There are no output/production roles — kaibo reasons
/// over a codebase and renders nothing. Perception (image input, audio-in later) is a
/// slot *capability* (`ModelCaps` / the `vision` pin), not a role. A cast may omit a
/// role: an absent slot means the capability is absent, not an error (the four
/// interactive built-in casts carry explorer+synth; the batch built-ins carry synth
/// only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelRole {
    Explorer,
    Synth,
}

impl ModelRole {
    /// Every role, in table order — the source for "known roles" error text.
    pub const ALL: [ModelRole; 2] = [Self::Explorer, Self::Synth];

    /// The role's config-table key (`[casts.<name>]`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Synth => "synth",
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

/// How a model slot runs. `None` on a slot means interactive (the default — the
/// synth answers inside a live tool loop). An offline lane returns a handle the
/// caller collects later:
/// - `Batch`  — the provider's async batch API (durable `backend/provider-id` handle).
/// - `Direct` — one long completion kaibo runs itself (a big local model taking the
///   time it takes); session-scoped `job-N` handle.
///
/// Lane lives on the **slot**, not the cast: a `deliberate` cast can pair an
/// interactive explorer with an offline synth, and the synth slot's lane is what
/// classifies the whole cast for the batch/interactive tool split (see
/// [`Config::cast_offline_lane`]). `Direct` is forward-declared here — validated,
/// parsed, and rendered — but no tool routes to it yet (see `server.rs`
/// `reject_offline_cast`/the roster split).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Batch,
    Direct,
}

impl std::str::FromStr for Lane {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "batch" => Ok(Self::Batch),
            "direct" => Ok(Self::Direct),
            other => Err(anyhow!("lane {other:?} is not one of batch|direct")),
        }
    }
}

impl Lane {
    /// The TOML spelling (`lane = "…"`), used in load-error messages and the
    /// `kaibo://config` render.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Batch => "batch",
            Self::Direct => "direct",
        }
    }
}

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
    /// Per-model system-prompt override: the role framing this *model* runs under,
    /// beside its other per-slot knobs. Full replace, like `[prompts]` (the kaish
    /// contract rides the `run_kaish` tool regardless). Resolved per phase by the
    /// server, winning over the per-phase `[prompts]` and the built-in. `None` →
    /// fall back to those. A per-call model override (a `bare` slot) carries none.
    pub preamble: Option<String>,
    /// How this slot runs: `None` is interactive (the default), `Some(Lane::Batch)`
    /// or `Some(Lane::Direct)` is offline — see [`Lane`]. Load validation
    /// (`Config::merge`) requires a lane to sit on a synth slot; an explorer with a
    /// lane is a loud error (the explorer always runs interactively).
    pub lane: Option<Lane>,
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
            preamble: None,
            lane: None,
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
    /// Key-file path, stored already `$VAR`/`~`-expanded (resolved once at load in
    /// `from_toml_str`). Used when the env var is unset/blank.
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

        // then a configured key file, *if it exists*. The path was `$VAR`/`~`-expanded
        // once at load (see `from_toml_str`), so it's used verbatim here.
        if let Some(file) = self.api_key_file.as_deref().map(PathBuf::from) {
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

    /// Whether this backend has a usable credential, judged *without* committing to a
    /// network call or pulling the secret into the answer path — the non-fatal sibling
    /// of [`resolve_key`](Self::resolve_key). It mirrors the same precedence (env var,
    /// then key file, then `key_optional`) but reports a verdict instead of returning
    /// the secret or erroring. The env lookup is injected so the classification is
    /// testable without touching the real environment.
    ///
    /// A *present-but-broken* key file still reads as [`KeyStatus::Present`] here: it's
    /// a configured credential, and `resolve_key` is the one that surfaces its breakage
    /// loudly at call time. We deliberately don't read the file's contents — existence
    /// is enough to say "the user set this up".
    pub fn key_status(&self, get_env: impl Fn(&str) -> Option<String>) -> KeyStatus {
        // env wins, when set and non-blank — same rule as resolve_key.
        if let Some(name) = self.api_key_env.as_deref() {
            if let Some(v) = get_env(name) {
                if !v.trim().is_empty() {
                    return KeyStatus::Present;
                }
            }
        }
        // then a configured key file, if it exists (contents unread — see above). The
        // path was `$VAR`/`~`-expanded once at load, so it's used verbatim here.
        if let Some(file) = self.api_key_file.as_deref().map(PathBuf::from) {
            if file.exists() {
                return KeyStatus::Present;
            }
        }
        if self.key_optional {
            KeyStatus::Placeholder
        } else {
            KeyStatus::Missing
        }
    }
}

/// A backend's credential availability, judged offline by [`Backend::key_status`].
/// The basis for the unconfigured-install setup guidance — never used to gate a call
/// (that's `resolve_key`'s loud, at-call-time job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    /// A key is configured: the env var is set and non-blank, or the key file exists.
    Present,
    /// No key, but the backend is keyless (`key_optional`) — a placeholder bearer is
    /// sent. Usable only if the endpoint is actually up, which we don't probe.
    Placeholder,
    /// No key and the backend requires one — a call would fail loudly at `resolve_key`.
    Missing,
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

    /// This cast's offline lane, read off its **synth** slot — the synth slot
    /// classifies the whole cast (an explorer always runs interactively, so its
    /// lane is never the question). `None` means no synth, or an interactive synth.
    pub fn synth_lane(&self) -> Option<Lane> {
        self.slot(ModelRole::Synth).and_then(|s| s.lane)
    }
}

/// Whether a cast can actually serve its model-backed tools, judged offline (no
/// network call) from its slots' backends — the basis for the unconfigured-install
/// setup guidance. `run_kaish` needs no backend, so it works in every state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastUsability {
    /// Every slot's backend has a configured key. The happy path — no nagging.
    Ready,
    /// No slot is outright missing a key, but at least one rides a keyless
    /// placeholder endpoint we haven't probed. Treated as configured (the user chose
    /// local, or set up some keys), so no setup banner — an unreachable endpoint
    /// still fails loudly at call time.
    LocalUnverified,
    /// At least one slot's backend needs a key and has none — the fresh install with
    /// nothing set up. The only state that lights up the setup guidance.
    Unconfigured,
}

// --- Telemetry -------------------------------------------------------------

/// Resolved OpenTelemetry export config. **Off by default**: kaibo reads private
/// source, and rig's GenAI spans carry prompts, completions, and source snippets,
/// so a default run must ship nothing off-box. Enabling opens an *outbound* OTLP/
/// HTTP connection to `endpoint` — allowed under the stdio-only invariant (kaibo
/// never *binds*), but a real boundary, hence opt-in. See `src/telemetry.rs`.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Whether to stand up the OTLP exporter at all. `false` → zero overhead.
    pub enabled: bool,
    /// OTLP/HTTP (protobuf) traces endpoint, e.g. `http://localhost:4318/v1/traces`.
    pub endpoint: String,
    /// Extra headers on each export request (auth for a remote collector, etc.).
    /// File-only — a header map has no clean single-env-var form.
    pub headers: BTreeMap<String, String>,
    /// Per-export deadline. The exporter must never wedge a shutdown flush.
    pub timeout: Duration,
    /// `service.name` on the OTLP Resource — how this process shows up in traces.
    pub service_name: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // The session's `otlp-mcp` collector and most local collectors listen
            // here; flipping `enabled` alone targets localhost, never a remote.
            endpoint: "http://localhost:4318/v1/traces".to_string(),
            headers: BTreeMap::new(),
            timeout: Duration::from_secs(10),
            service_name: "kaibo".to_string(),
        }
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
    /// Follow git worktrees of an already-allowed repo (default `true`). When a
    /// per-call `path` misses the static allowed set, admit it if it is a linked
    /// worktree of a repo already in the set — resolved by reading git's link files,
    /// never by running git (see `crate::worktree`). Set `false` (via
    /// `--no-follow-worktrees`, `KAIBO_NO_FOLLOW_WORKTREES`, or
    /// `[server] follow_worktrees = false`) to keep the boundary strictly static.
    pub follow_worktrees: bool,
    /// Default cast name when a call omits `cast`.
    pub default_cast: String,
    /// `EnvFilter` directive used when `RUST_LOG` is unset.
    pub log: String,
    /// Which tools to advertise.
    pub tools: ToolGating,
    pub defaults: Defaults,
    /// OpenTelemetry export — off by default (see [`TelemetryConfig`]).
    pub telemetry: TelemetryConfig,
    /// House-rules files spliced into each consultation tool's preamble (the
    /// `[context]` table). Defaults to reading `AGENTS.md` when present.
    pub context: ContextConfig,
    /// Per-phase system-prompt overrides (the `[prompts]` table). Empty by
    /// default — the built-in preambles run unchanged.
    pub prompts: PromptOverrides,
    /// Static repo-orientation injected into the exploring preamble (the
    /// `[orientation]` table). On by default for small repos.
    pub orientation: OrientationConfig,
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

    /// The offline lane `name` (a canonical cast name) runs on, read off its synth
    /// slot (see [`Cast::synth_lane`]). `None` means no synth, or an interactive
    /// synth. A name not in the registry reads as `None` (the caller already worked
    /// from canonical keys).
    pub fn cast_offline_lane(&self, name: &str) -> Option<Lane> {
        self.casts.get(name).and_then(Cast::synth_lane)
    }

    /// Whether canonical cast `name` serves the **interactive answer** tools (`consult`,
    /// `consult_submit`, `oneshot`) — i.e. its synth runs interactively (or it carries no
    /// synth at all). The mirror of `reject_offline_cast`'s acceptance: an offline synth
    /// belongs to `batch_submit`/`deliberate`, not these. (`explore` is deliberately *not*
    /// here — it runs only the explorer, so it takes any cast with one via
    /// [`cast_can_explore`](Self::cast_can_explore), interactive or not.) One of the per-tool
    /// cast predicates the enum roster and the gates share (see `server.rs::CAST_ENUM_RULES`).
    pub fn cast_is_interactive(&self, name: &str) -> bool {
        self.cast_offline_lane(name).is_none()
    }

    /// Whether `name` is declared for the batch lane specifically. Used to partition
    /// the live roster onto the right tools' `cast` enums — batch casts to
    /// `batch_submit`, the rest to the interactive tools.
    pub fn cast_is_batch(&self, name: &str) -> bool {
        self.cast_offline_lane(name) == Some(Lane::Batch)
    }

    /// Whether canonical cast `name` can staff a `deliberate` call: an **offline
    /// synth** (batch *or* direct lane) paired with an **explorer** slot to build the
    /// dossier. A synth-only batch cast (no explorer — `gemini-batch`/`anthropic-batch`)
    /// can't: it has no dossier phase. An interactive cast (no offline synth) belongs to
    /// `consult`, not here. This is the third roster partition the per-slot lane reshape
    /// enabled — and the one that finally routes a `Direct` synth (unreachable until
    /// `deliberate` shipped).
    pub fn cast_can_deliberate(&self, name: &str) -> bool {
        self.casts.get(name).is_some_and(|c| {
            c.synth_lane().is_some() && c.slot(ModelRole::Explorer).is_some()
        })
    }

    /// Whether canonical cast `name` can staff an `explore` call: it carries an **explorer**
    /// slot — the *only* thing `explore` runs. Independent of the synth lane, because
    /// `explore` never touches the synth: a `deliberate`/`direct` cast's explorer is as valid
    /// as an interactive cast's (an explorer always runs interactively by construction). So
    /// `explore` advertises *more* casts than the interactive tools — pointing it at a
    /// deliberate cast runs that team's (often smarter) explorer standalone, handy for
    /// evaluating the explorer or for a stronger sweep than the caller's own. A synth-only
    /// cast (no explorer — `gemini-batch`/`oneshot`-only casts) can't, and is correctly
    /// left out (it would fault at the explorer-arm resolve).
    pub fn cast_can_explore(&self, name: &str) -> bool {
        self.casts
            .get(name)
            .is_some_and(|c| c.slot(ModelRole::Explorer).is_some())
    }

    /// Whether canonical cast `name` is the configured default — comparing against the
    /// *resolved* default, so an alias default (`server.cast = "claude"`) still matches
    /// its canonical cast (`anthropic`). The roster renderer (`casts_section`) gets
    /// canonical names from [`usable_casts`](Self::usable_casts),
    /// so a bare `name == default_cast` string compare would silently drop the
    /// `(default)` tag whenever the default was set by an alias. An unresolvable default
    /// (can't happen post-load, validated there) reads as "nothing is default".
    pub fn is_default_cast(&self, name: &str) -> bool {
        self.resolve_cast(&self.default_cast)
            .is_ok_and(|c| c.name == name)
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

    /// Judge whether `cast` can serve its model-backed tools, *without* a network
    /// call — the basis for the unconfigured-install setup guidance. A cast is only
    /// [`CastUsability::Unconfigured`] when a slot's backend needs a key and has none;
    /// one keyless local endpoint reads as [`CastUsability::LocalUnverified`]
    /// (configured, just unprobed) so an env-only or deliberate-local setup is never
    /// nagged. Partial setup counts as broken: *any* slot missing a key wins. The env
    /// lookup is injected for testability. An empty cast (no slots) reads as `Ready`
    /// here — its missing roles fail loudly per-tool at call time, not our concern.
    pub fn cast_usability(
        &self,
        cast: &Cast,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> CastUsability {
        let mut any_placeholder = false;
        for slot in cast.slots.values() {
            // A slot's backend ref resolved at merge time, so this lookup can't fail;
            // skip defensively rather than panic if that invariant ever changes.
            let Ok(backend) = self.resolve_backend(&slot.backend) else {
                continue;
            };
            match backend.key_status(&get_env) {
                KeyStatus::Missing => return CastUsability::Unconfigured,
                KeyStatus::Placeholder => any_placeholder = true,
                KeyStatus::Present => {}
            }
        }
        if any_placeholder {
            CastUsability::LocalUnverified
        } else {
            CastUsability::Ready
        }
    }

    /// The casts that can reach a model *right now* — [`CastUsability::Ready`] or
    /// [`CastUsability::LocalUnverified`], with the state so the handshake can tag a
    /// local/unverified one. Sorted by name (the `casts` `BTreeMap`'s order). This is
    /// the truthful, startup-resolved answer to "what can I pass as `cast`?" — and it
    /// includes config.toml casts the static per-tool `cast` enum can't name. An
    /// [`CastUsability::Unconfigured`] cast (a backend missing its key) is filtered
    /// out: we advertise only what will actually work. Env lookup is injected for
    /// testability, mirroring [`cast_usability`](Self::cast_usability).
    pub fn usable_casts(
        &self,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Vec<(String, CastUsability)> {
        self.casts
            .iter()
            .filter_map(|(name, cast)| match self.cast_usability(cast, &get_env) {
                CastUsability::Unconfigured => None,
                usable => Some((name.clone(), usable)),
            })
            .collect()
    }

    /// [`cast_usability`](Self::cast_usability) for the default cast — what `get_info`
    /// reads to decide whether to surface setup guidance. If the default cast somehow
    /// doesn't resolve (it's validated to at startup), treat it as `Unconfigured` so we
    /// guide rather than pretend everything is fine.
    pub fn default_cast_usability(
        &self,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> CastUsability {
        match self.resolve_cast(&self.default_cast) {
            Ok(cast) => self.cast_usability(cast, get_env),
            Err(_) => CastUsability::Unconfigured,
        }
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
                    // belongs to the built-in `openai-local` backend alone —
                    // inherited here it would silently dial the wrong server and fail
                    // as a cryptic 404 mid-call instead of loudly at load.
                    if b.kind == ProviderKind::Openai && b.base_url.is_none() {
                        bail!(
                            "backend {name:?} (kind \"openai\") must set base_url — \
                             only the built-in `openai-local` backend falls back to \
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
            // A backend name may never contain '/'. Two wire formats split on the
            // first slash and trust the prefix to be slash-free: the `"backend/model-id"`
            // slot ref (`split_backend_model`) and the `"backend/provider-id"` batch
            // handle (`server.rs::parse_batch_handle`). A slash in a backend name would
            // silently mis-route both. Enforce the invariant the parsers rely on rather
            // than trusting convention.
            if b.name.contains('/') {
                bail!(
                    "backend name {:?} may not contain '/': backend names prefix slot \
                     refs (\"backend/model-id\") and batch handles (\"backend/provider-id\"), \
                     which split on the first slash",
                    b.name
                );
            }
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

        // Expand `$VAR`/`${VAR}` and a leading `~` in each backend's `api_key_file` — the
        // same uniform rule `root`/`allow_paths` get, so a portable `$XDG_CONFIG_HOME/key`
        // (or the built-in `~/.gemini-api-key`) resolves per-environment. Done once here,
        // loudly on an undefined/empty/non-UTF-8 variable, so the use-sites
        // (`resolve_key`/`key_status`) read an already-resolved path and stay infallible —
        // `key_status` feeds the offline cast-usability classifiers, which can't take a
        // `Result`. Absolute/plain paths pass through unchanged.
        for b in backends.values_mut() {
            if let Some(f) = b.api_key_file.as_deref() {
                let expanded = expand_path(f)?;
                let what = format!("backend {:?} api_key_file", b.name);
                b.api_key_file = Some(expanded_to_utf8(expanded, &what)?);
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
                let mut slot = raw_slot
                    .clone()
                    .into_slot()
                    .with_context(|| format!("cast {name:?} {} slot", role.key()))?;
                // A slot's `lane` is sticky across a bare re-declaration of its model:
                // retuning `gemini-batch`'s synth id without repeating `lane = "batch"`
                // must not silently revert it to interactive — mirrors the old
                // cast-level `batch` flag's stickiness, now scoped to the slot that
                // actually carries the lane. An explicit `lane` on the new declaration
                // (or no prior slot at this role) always wins outright.
                if slot.lane.is_none() {
                    if let Some(prev) = cast.slots.get(&role) {
                        slot.lane = prev.lane;
                    }
                }
                cast.slots.insert(role, slot);
            }
            // Backward-compat sugar: `batch = true` at cast level normalizes onto the
            // synth slot's `lane` — there is exactly ONE internal representation of
            // lane (the slot field); this just translates the old spelling into it.
            // Applied after the slot merge above so it sees this stanza's synth slot.
            // `batch = false` (or omitting the key) is a no-op, not a clear — there's no
            // "un-batch" flag, only the slot's own `lane`.
            if let Some(true) = rc.batch {
                let synth = cast.slots.get_mut(&ModelRole::Synth).ok_or_else(|| {
                    anyhow!(
                        "cast {name:?}: batch = true needs a synth slot \
                         (batch_submit runs the synth model)"
                    )
                })?;
                match synth.lane {
                    None => synth.lane = Some(Lane::Batch),
                    Some(Lane::Batch) => {} // idempotent
                    Some(other) => bail!(
                        "cast {name:?}: `batch = true` conflicts with the synth slot's \
                         `lane = {:?}`",
                        other.as_str()
                    ),
                }
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
                // A slot's lane needs a fitting role and backend, caught here rather
                // than as a baffling refusal or a 400 at submit/deliberate time. The
                // explorer always runs interactively (a `deliberate` cast pairs it with
                // an *offline* synth, never the other way round), so a lane on an
                // explorer slot is a loud error regardless of kind. `Batch` needs the
                // provider's own async batch API; `Direct` (kaibo running one long
                // completion itself) accepts any backend that resolves at all — no
                // "batch cast must be synth-only" constraint here, so an interactive
                // explorer may freely sit beside an offline synth (the `deliberate`
                // shape); the built-in batch casts stay synth-only by construction, not
                // by a rule enforced here.
                if let Some(lane) = slot.lane {
                    if *role != ModelRole::Synth {
                        bail!(
                            "cast {name:?}: the {} slot declares lane = {:?}, but only a \
                             synth slot may run offline — the explorer always runs \
                             interactively",
                            role.key(),
                            lane.as_str()
                        );
                    }
                    if lane == Lane::Batch && !crate::batch::batch_supported(kind) {
                        bail!(
                            "cast {name:?}: synth lane = \"batch\" but the synth backend \
                             {:?} ({}) has no batch API — a batch synth must sit on a \
                             batch-capable backend ({})",
                            slot.backend,
                            kind.canonical_name(),
                            crate::batch::supported_kinds_list(),
                        );
                    }
                    // `Direct` needs nothing further: the backend already resolved above,
                    // and a direct completion has no provider-side capability to check —
                    // it's kaibo driving one ordinary call itself, just slower.
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
        // Expand `$VAR`/`${VAR}` and a leading `~` in `root` / `allow_paths` (file *and*
        // env layers both land here as strings), so a hand-edited `~/src` or the portable
        // `$TMPDIR` / `$XDG_RUNTIME_DIR/scratch` resolves per-environment rather than a
        // literal token that fails canonicalization at startup. An undefined variable is a
        // loud load error (`expand_path`), not a silent empty segment. Absolute/relative
        // paths with no `~`/`$` pass through unchanged.
        let root = server.root.as_deref().map(expand_path).transpose()?;
        let allow_paths = server
            .allow_paths
            .unwrap_or_default()
            .iter()
            .map(|s| expand_path(s))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let follow_worktrees = server.follow_worktrees.unwrap_or(true);
        let telemetry = merge_telemetry(raw.telemetry.unwrap_or_default())?;
        let context = merge_context(raw.context.unwrap_or_default())?;
        let prompts = merge_prompts(raw.prompts.unwrap_or_default())?;
        let orientation = merge_orientation(raw.orientation.unwrap_or_default())?;
        let mut sandbox = merge_sandbox(raw.sandbox.unwrap_or_default());
        // The ignore policy lives in its own `[kaish.ignore]` stanza (behavior, not
        // safety), but resolves onto the same struct the kernel builder consumes.
        sandbox.ignore = merge_kaish(raw.kaish.unwrap_or_default())?;
        // A zero scratch budget rejects *every* write to the `/` MemoryFs — mktemp,
        // every redirect — which breaks normal explorer operation, so it's never a
        // real intent (there's no "unbounded" escape hatch by design: an unbounded
        // scratch is the bug this cap exists to prevent). Loud at load, not as a
        // baffling StorageFull on the first redirect.
        if sandbox.scratch_limit_bytes == 0 {
            bail!(
                "[sandbox] scratch_limit_bytes must be > 0 — a zero budget refuses \
                 every scratch write (mktemp, redirects); lower it to bound RAM, but \
                 not to zero"
            );
        }

        let cfg = Self {
            root,
            allow_paths,
            follow_worktrees,
            default_cast,
            log,
            tools,
            defaults,
            telemetry,
            context,
            prompts,
            orientation,
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
    // A thin positional overlay of the parsed CLI flags onto the loaded config — the
    // knobs are inherently many and bundling them into a struct would just relocate
    // the positionality into a literal, so we accept the arg count here.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_cli(
        &mut self,
        root: Option<PathBuf>,
        cast: Option<String>,
        disable: ToolDisables,
        allow_paths: Vec<PathBuf>,
        disable_follow_worktrees: bool,
        project_context_files: Vec<String>,
        user_context_files: Vec<PathBuf>,
    ) {
        if let Some(root) = root {
            self.root = Some(root);
        }
        // Mirrors the `--no-<tool>` discipline: the CLI can only turn the follow OFF,
        // never force it on over a `[server] follow_worktrees = false` in the file.
        if disable_follow_worktrees {
            self.follow_worktrees = false;
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
        if disable.deliberate {
            self.tools.deliberate = false;
        }
        if disable.oneshot {
            self.tools.oneshot = false;
        }
        if disable.run_kaish {
            self.tools.run_kaish = false;
        }
        if disable.batch {
            self.tools.batch = false;
        }
        // Non-empty CLI allow_paths replaces lower layers (env/file).
        if !allow_paths.is_empty() {
            self.allow_paths = allow_paths;
        }
        // Each context list replaces its lower layers when the operator passed any
        // on the CLI — matching the allow_paths discipline. The CLI has no way to
        // express "empty", so passing a flag is always an additive override, never
        // the opt-out (that's KAIBO_PROJECT_FILES= or an explicit [] in the file).
        if !project_context_files.is_empty() {
            self.context.project_files = project_context_files;
        }
        if !user_context_files.is_empty() {
            self.context.user_files = user_context_files;
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
    pub deliberate: bool,
    pub oneshot: bool,
    pub run_kaish: bool,
    pub batch: bool,
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
        // The local-default built-in. `openai` is *not* an alias: that's the wire
        // protocol's id, and leaving it free lets a user name their own backend
        // `[backends.openai]` (e.g. the hosted API) without an alias collision.
        "openai-local" => &["local", "lemonade", "gemma", "gemma4"],
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
        b.name = kind.builtin_name().to_string();
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
        let name = kind.builtin_name().to_string();
        let (explorer, synth) = default_models(kind);
        // Both agent roles are seeded. A cast may omit one in config — absent means
        // the capability is absent, and nothing downstream errors on that.
        let slots = BTreeMap::from([
            (ModelRole::Explorer, ModelSlot::bare(&name, explorer)),
            (ModelRole::Synth, ModelSlot::bare(&name, synth)),
        ]);
        m.insert(name.clone(), Cast { name, slots });
    }
    // The offline batch lane's built-in casts (synth slot `lane = Batch`): staffed by
    // a single big, slow, capable synth — the model whose latency is *free* in batch
    // but near-unusable interactively. They carry synth only, because `batch_submit` is
    // a toolless oneshot (no explorer sweep). A *user* cast may pair an interactive
    // explorer with an offline synth — that's the `deliberate` shape, valid since the
    // per-slot lane reshape — but these built-ins have no dossier phase to staff, so an
    // explorer slot here would just be dead weight.
    //
    // gemini-batch synths Gemini Pro via the `gemini-pro-latest` *alias*, deliberately
    // not a pinned preview id: pinned Pro previews get retired out from under us (a live
    // batch dogfood caught `gemini-3-pro-preview` 404ing mid-flight — submit accepted it,
    // the per-item request failed at run time), so the latest-alias is drift-resistant.
    let gemini_batch = "gemini-batch".to_string();
    m.insert(
        gemini_batch.clone(),
        Cast {
            name: gemini_batch,
            slots: BTreeMap::from([(
                ModelRole::Synth,
                ModelSlot {
                    lane: Some(Lane::Batch),
                    ..ModelSlot::bare("gemini", "gemini-pro-latest")
                },
            )]),
        },
    );
    // anthropic-batch synths Claude Opus — the Anthropic analogue of Gemini Pro, the big
    // model whose batch latency is free. Anthropic has no `-latest` alias convention, so
    // the id is pinned (`claude-opus-4-8`); when it's retired, bump it here or pin a
    // current Opus in config.toml.
    let anthropic_batch = "anthropic-batch".to_string();
    m.insert(
        anthropic_batch.clone(),
        Cast {
            name: anthropic_batch,
            slots: BTreeMap::from([(
                ModelRole::Synth,
                ModelSlot {
                    lane: Some(Lane::Batch),
                    ..ModelSlot::bare("anthropic", "claude-opus-4-8")
                },
            )]),
        },
    );
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
    telemetry: Option<RawTelemetry>,
    context: Option<RawContext>,
    prompts: Option<RawPrompts>,
    orientation: Option<RawOrientation>,
    sandbox: Option<RawSandbox>,
    kaish: Option<RawKaish>,
    backends: Option<BTreeMap<String, RawBackend>>,
    casts: Option<BTreeMap<String, RawCast>>,
    /// Tombstone. `[profiles]` was split into `[backends]` + `[casts]`
    /// (docs/casts.md); its presence — any shape — is a load error in `merge`.
    /// Declared so the message points at the migration instead of serde's
    /// generic unknown-field complaint.
    profiles: Option<toml::Value>,
}

/// The `[telemetry]` stanza — mirrors `[server]` in spirit: all optional, overrides
/// the off-by-default built-in. `headers` is a sub-table; the rest take `KAIBO_*`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetry {
    enabled: Option<bool>,
    endpoint: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    timeout_secs: Option<u64>,
    service_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    exec_timeout_secs: Option<u64>,
    output_limit_bytes: Option<usize>,
    /// Cap on the `/` scratch MemoryFs. Env: `KAIBO_SCRATCH_LIMIT_BYTES`.
    scratch_limit_bytes: Option<u64>,
    /// Builtins to disable on top of the read-only denylist (file-only; no env).
    disable_builtins: Option<Vec<String>>,
}

/// The `[kaish]` stanza — tuning for the kaish kernel's *behavior* (as opposed to
/// `[sandbox]`, which is its safety boundary and resource limits). Today it carries
/// only the ignore policy; it's a stanza, not a bare key, so future kaish knobs have
/// a home. File-only (no `KAIBO_*` env), like its `[sandbox]` list siblings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKaish {
    ignore: Option<RawIgnore>,
}

/// The `[kaish.ignore]` sub-table — which gitignore-format files the file-walking
/// builtins honor, and how broadly. Every key is optional and defaults to
/// [`IgnoreConfig::agent`]'s value, so an absent stanza (or a partial one) preserves
/// today's `.gitignore`-aware, enforced-scope behavior. `files` *replaces* the
/// default `[".gitignore"]` when given — list it explicitly alongside extras to keep
/// it (though `auto_gitignore`, on by default, still walks nested `.gitignore`s
/// regardless).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIgnore {
    /// Ignore filenames loaded at the root and walked up through ancestors, in
    /// precedence order (later wins, ripgrep-style). Default: `[".gitignore"]`.
    files: Option<Vec<String>>,
    /// Apply built-in defaults (`target/`, `node_modules/`, `.git`). Default: true.
    defaults: Option<bool>,
    /// Auto-load nested `.gitignore` files during the walk. Default: true.
    auto_gitignore: Option<bool>,
    /// Also honor the user's global gitignore (`core.excludesFile`). Default: false.
    global_gitignore: Option<bool>,
    /// `"enforced"` (all walkers, incl. `find` — protects the agent's context) or
    /// `"advisory"` (polite tools only; `find` stays POSIX-unrestricted). Default:
    /// `"enforced"`.
    scope: Option<String>,
}

/// The `[context]` stanza — files whose contents are spliced into each
/// consultation tool's preamble as standing guidance. Both lists optional;
/// omitting `project_files` keeps the built-in `["AGENTS.md"]` default, while an
/// explicit empty list opts out of even that. See [`ContextConfig`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContext {
    /// Root-relative files read if present. Env: `KAIBO_PROJECT_FILES`
    /// (colon-separated). Default (when the key is absent): `["AGENTS.md"]`.
    project_files: Option<Vec<String>>,
    /// Absolute/tilde files read unconditionally (missing = error). Env:
    /// `KAIBO_USER_FILES` (colon-separated). Default: empty.
    user_files: Option<Vec<String>>,
}

/// The `[prompts]` stanza — per-phase system-prompt (preamble) overrides. Each
/// is the full role framing, verbatim (the kaish contract rides the `run_kaish`
/// tool independently). All optional; an absent table runs the built-ins. File-
/// only: multiline prose has no clean env/CLI form (same call as
/// `telemetry.headers`). See [`PromptOverrides`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrompts {
    /// Replaces the explorer preamble (the nested `explore′` sweep inside `consult`).
    explorer: Option<String>,
    /// Replaces the `consult` driver preamble.
    consult: Option<String>,
    /// Replaces the toolless `oneshot` preamble.
    oneshot: Option<String>,
    /// Replaces the offline, max-thinking `batch_submit` preamble.
    batch: Option<String>,
}

/// The `[orientation]` stanza — the static repo-map injected into the exploring
/// preamble. All fields optional; defaults are on with a 256-file ceiling and a
/// 4-level fallback tree. See [`OrientationConfig`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrientation {
    /// Inject the map at all. Default `true`.
    enabled: Option<bool>,
    /// File-count ceiling for the full-list form; over it, the map falls back to a
    /// directory tree. Default `256`.
    full_list_max_files: Option<usize>,
    /// How many directory levels the fallback tree descends. Default `4`.
    tree_max_depth: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    root: Option<String>,
    /// Additional allowed path trees: a per-call `path` must canonicalize to
    /// at-or-under `root` OR one of these. Env override: `KAIBO_ALLOW_PATHS`
    /// (colon-separated). CLI override: repeatable `--allow-path DIR`.
    allow_paths: Option<Vec<String>>,
    /// Follow git worktrees of an already-allowed repo. Default `true`. Env:
    /// `KAIBO_NO_FOLLOW_WORKTREES`. CLI: `--no-follow-worktrees`.
    follow_worktrees: Option<bool>,
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
    deliberate: Option<bool>,
    oneshot: Option<bool>,
    run_kaish: Option<bool>,
    batch: Option<bool>,
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
    job_capacity: Option<usize>,
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
    /// Backward-compat sugar for the synth slot's `lane = "batch"` (`batch_submit`
    /// only). `Some(true)` normalizes onto the synth slot's [`Lane`] at merge time —
    /// see the cast-merge loop in [`Config::merge`]; there is exactly one internal
    /// representation of lane (the slot field), this is purely an input spelling.
    /// `Some(false)` and absent are both no-ops — there is no "un-batch" flag, only
    /// the slot's own `lane`.
    batch: Option<bool>,
    explorer: Option<RawSlot>,
    synth: Option<RawSlot>,
}

impl RawCast {
    /// The configured (role, slot) pairs, in role order.
    fn slots(&self) -> Vec<(ModelRole, &RawSlot)> {
        [
            (ModelRole::Explorer, &self.explorer),
            (ModelRole::Synth, &self.synth),
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
    preamble: Option<String>,
    /// How this slot runs — `"batch"` or `"direct"`; absent means interactive. See
    /// [`Lane`]. Only meaningful on a synth slot; an explorer slot with a lane is a
    /// loud load error (checked in `Config::merge`, after backend resolution).
    lane: Option<String>,
}

impl RawSlot {
    fn into_slot(self) -> Result<ModelSlot> {
        match self {
            Self::Ref(s) => {
                let (backend, id) = parse_slot_ref(&s)?;
                Ok(ModelSlot::bare(backend, id))
            }
            // Trim `backend`/`id` so the table form behaves like the `"backend/id"`
            // string form (which trims in `parse_slot_ref`): identical intent must
            // not depend on the spelling. The empty-after-trim case is still caught
            // loudly downstream (`merge`: unknown backend / empty model id).
            Self::Table(t) => Ok(ModelSlot {
                backend: t.backend.trim().to_string(),
                id: t.id.trim().to_string(),
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
                lane: t.lane.as_deref().map(str::parse).transpose().context("lane")?,
                // Same loud-on-empty rule as `[prompts]`: a blank per-model prompt
                // is never intended, and silently running it would strip the role
                // framing with no signal. Drop the key to fall back.
                preamble: match t.preamble {
                    Some(p) if p.trim().is_empty() => bail!(
                        "slot preamble is empty — a blank system prompt is never \
                         intended; remove the key to use the built-in/`[prompts]` value"
                    ),
                    other => other,
                },
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
    // Same zero-is-never-intent reasoning as session_capacity: a zero cap can't build
    // an LruCache and would mean "hold no jobs", which defeats `consult_submit`.
    let job_capacity = match raw.job_capacity {
        Some(n) => NonZeroUsize::new(n)
            .ok_or_else(|| anyhow!("[defaults] job_capacity must be > 0 (got 0)"))?,
        None => d.job_capacity,
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
        job_capacity,
    })
}

fn merge_telemetry(raw: RawTelemetry) -> Result<TelemetryConfig> {
    let d = TelemetryConfig::default();
    // A zero timeout means "time out instantly" — never a real intent, and it would
    // brick every export (and the shutdown flush). Catch it at load, same spirit as
    // the backend request_timeout check.
    let timeout = match raw.timeout_secs {
        Some(0) => {
            bail!("[telemetry] timeout_secs must be > 0 — a zero deadline fails every export")
        }
        Some(n) => Duration::from_secs(n),
        None => d.timeout,
    };
    Ok(TelemetryConfig {
        enabled: raw.enabled.unwrap_or(d.enabled),
        endpoint: raw.endpoint.unwrap_or(d.endpoint),
        headers: raw.headers.unwrap_or(d.headers),
        timeout,
        service_name: raw.service_name.unwrap_or(d.service_name),
    })
}

/// Resolve `[context]`. A *missing* `project_files` keeps the built-in
/// `["AGENTS.md"]` default (vendor-neutral, opt-out by an explicit empty list);
/// `user_files` default to empty and are `$VAR`/`~`-expanded here (via `expand_path`,
/// the same uniform rule `root`/`allow_paths` get) so `assemble` is pure filesystem
/// work and a portable `$XDG_CONFIG_HOME/notes.md` resolves per-environment. An
/// undefined/empty/non-UTF-8 variable is a loud load error, not a silent gap. The files
/// themselves are read lazily, per call, against the resolved root — not here — so a
/// config load never touches a project.
fn merge_context(raw: RawContext) -> anyhow::Result<ContextConfig> {
    Ok(ContextConfig {
        project_files: raw
            .project_files
            .unwrap_or_else(|| vec!["AGENTS.md".to_string()]),
        user_files: raw
            .user_files
            .unwrap_or_default()
            .iter()
            .map(|s| expand_path(s))
            .collect::<anyhow::Result<Vec<_>>>()?,
    })
}

/// Resolve `[prompts]`. Each override is taken verbatim (full replace), but an
/// empty or whitespace-only value is refused loudly: a blank system prompt is
/// never the intent, and silently running it would strip the role framing with no
/// signal. Remove the key to fall back to the built-in.
fn merge_prompts(raw: RawPrompts) -> Result<PromptOverrides> {
    fn non_empty(label: &str, v: Option<String>) -> Result<Option<String>> {
        match v {
            Some(s) if s.trim().is_empty() => bail!(
                "[prompts] {label} is empty — a blank system prompt is never intended; \
                 remove the key to use the built-in preamble"
            ),
            other => Ok(other),
        }
    }
    Ok(PromptOverrides {
        explorer: non_empty("explorer", raw.explorer)?,
        consult: non_empty("consult", raw.consult)?,
        oneshot: non_empty("oneshot", raw.oneshot)?,
        batch: non_empty("batch", raw.batch)?,
    })
}

/// Resolve `[orientation]`. A zero `full_list_max_files` would refuse *every* repo
/// (no project has zero files), which is never the intent — disable it via
/// `enabled = false` instead. A zero `tree_max_depth` would render an empty
/// directory map, which is equally pointless. Both are loud at load rather than a
/// baffling per-call result.
fn merge_orientation(raw: RawOrientation) -> Result<OrientationConfig> {
    let d = OrientationConfig::default();
    let full_list_max_files = raw.full_list_max_files.unwrap_or(d.full_list_max_files);
    if full_list_max_files == 0 {
        bail!(
            "[orientation] full_list_max_files must be > 0 — a zero ceiling refuses \
             every repo; set `enabled = false` to turn the map off instead"
        );
    }
    let tree_max_depth = raw.tree_max_depth.unwrap_or(d.tree_max_depth);
    if tree_max_depth == 0 {
        bail!(
            "[orientation] tree_max_depth must be > 0 — a zero depth renders an empty \
             directory map; set `enabled = false` to turn the map off instead"
        );
    }
    Ok(OrientationConfig {
        enabled: raw.enabled.unwrap_or(d.enabled),
        full_list_max_files,
        tree_max_depth,
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
        scratch_limit_bytes: raw.scratch_limit_bytes.unwrap_or(d.scratch_limit_bytes),
        disable_builtins: raw.disable_builtins.unwrap_or(d.disable_builtins),
        // Resolved separately from `[kaish.ignore]` and stitched in by `merge`.
        ignore: d.ignore,
    }
}

/// Resolve `[kaish.ignore]` into the kernel's [`IgnoreConfig`]. Each key falls back
/// to the [`IgnoreConfig::agent`] default, so an absent stanza reproduces it exactly
/// (`.gitignore` + built-in defaults, enforced scope) and a partial stanza overrides
/// only the keys it names. An unrecognized `scope` is a load error, not a silent
/// fallback to the default.
fn merge_kaish(raw: RawKaish) -> Result<IgnoreConfig> {
    let ri = raw.ignore.unwrap_or_default();
    let scope = match ri.scope.as_deref() {
        None | Some("enforced") => IgnoreScope::Enforced,
        Some("advisory") => IgnoreScope::Advisory,
        Some(other) => {
            bail!("[kaish.ignore] scope = {other:?} must be \"enforced\" or \"advisory\"")
        }
    };

    let mut ignore = IgnoreConfig::none();
    ignore.set_scope(scope);
    ignore.set_defaults(ri.defaults.unwrap_or(true));
    ignore.set_auto_gitignore(ri.auto_gitignore.unwrap_or(true));
    ignore.set_use_global_gitignore(ri.global_gitignore.unwrap_or(false));
    for name in ri.files.unwrap_or_else(|| vec![".gitignore".to_string()]) {
        ignore.add_file(&name);
    }
    Ok(ignore)
}

fn merge_tools(raw: RawTools) -> ToolGating {
    let d = ToolGating::default();
    ToolGating {
        consult: raw.consult.unwrap_or(d.consult),
        explore: raw.explore.unwrap_or(d.explore),
        deliberate: raw.deliberate.unwrap_or(d.deliberate),
        oneshot: raw.oneshot.unwrap_or(d.oneshot),
        run_kaish: raw.run_kaish.unwrap_or(d.run_kaish),
        batch: raw.batch.unwrap_or(d.batch),
    }
}

// --- Env overrides ---------------------------------------------------------

/// Fold `KAIBO_*` env vars into the raw config (env over file). Numeric parse
/// failures are loud — a `KAIBO_MAX_TOKENS=abc` is a mistake, not a fallback.
/// Split a colon-separated path list (PATH grammar) into entries, dropping empty
/// components. Shared by the context-file env vars; an empty input yields an empty
/// list (the opt-out signal for `project_files`).
fn split_path_list(v: &str) -> Vec<String> {
    std::env::split_paths(v)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

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
    if env_flag(get, "KAIBO_NO_FOLLOW_WORKTREES") {
        server.follow_worktrees = Some(false);
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
    if env_flag(get, "KAIBO_NO_DELIBERATE") {
        tools.deliberate = Some(false);
    }
    if env_flag(get, "KAIBO_NO_ONESHOT") {
        tools.oneshot = Some(false);
    }
    if env_flag(get, "KAIBO_NO_RUN_KAISH") {
        tools.run_kaish = Some(false);
    }
    if env_flag(get, "KAIBO_NO_BATCH") {
        tools.batch = Some(false);
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
    if let Some(v) = get("KAIBO_JOB_CAPACITY") {
        defaults.job_capacity = Some(parse_env_int("KAIBO_JOB_CAPACITY", &v)?);
    }

    // Context files: colon-separated like PATH (and like KAIBO_ALLOW_PATHS), so a
    // single path with no colon is one entry. An empty value sets an empty list —
    // that's the opt-out for project_files (turn off the AGENTS.md default), so we
    // set Some(empty) rather than skipping.
    let context = raw.context.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_PROJECT_FILES") {
        context.project_files = Some(split_path_list(&v));
    }
    if let Some(v) = get("KAIBO_USER_FILES") {
        context.user_files = Some(split_path_list(&v));
    }

    let sandbox = raw.sandbox.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_EXEC_TIMEOUT_SECS") {
        sandbox.exec_timeout_secs = Some(parse_env_int("KAIBO_EXEC_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_OUTPUT_LIMIT_BYTES") {
        sandbox.output_limit_bytes = Some(parse_env_int("KAIBO_OUTPUT_LIMIT_BYTES", &v)?);
    }
    if let Some(v) = get("KAIBO_SCRATCH_LIMIT_BYTES") {
        sandbox.scratch_limit_bytes = Some(parse_env_int("KAIBO_SCRATCH_LIMIT_BYTES", &v)?);
    }
    // disable_builtins is a list — file-only, no env form.

    let telemetry = raw.telemetry.get_or_insert_with(Default::default);
    if let Some(v) = get("KAIBO_TELEMETRY_ENABLED") {
        // Same on/off grammar as the KAIBO_NO_* flags, but here it can flip a
        // file-enabled exporter *off* too — so set the parsed bool either way.
        let on = {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        };
        telemetry.enabled = Some(on);
    }
    if let Some(v) = get("KAIBO_TELEMETRY_ENDPOINT") {
        telemetry.endpoint = Some(v);
    }
    if let Some(v) = get("KAIBO_TELEMETRY_TIMEOUT_SECS") {
        telemetry.timeout_secs = Some(parse_env_int("KAIBO_TELEMETRY_TIMEOUT_SECS", &v)?);
    }
    if let Some(v) = get("KAIBO_TELEMETRY_SERVICE_NAME") {
        telemetry.service_name = Some(v);
    }
    // headers is a map — file-only, no env form (same call as disable_builtins).
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

/// Expand a leading `~` or `~/...` to `$HOME`. Leaves every other path untouched —
/// in particular `~user` (no slash) is **not** expanded: kaibo never does a `getpwnam`
/// lookup from config-parsing code, and a literal `~user` simply fails canonicalization
/// loudly. Keep it this narrow; widening it to `~user` would pull a mutable system
/// database into a path that feeds the containment boundary. With `$HOME` unset the
/// input passes through verbatim (and then fails canonicalization), never silently.
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

/// Expand `$VAR` / `${VAR}` references *and* a leading `~`, in that order — the
/// portable path form for `root` / `allow_paths`. An undefined variable is a **loud
/// error**, never a silent empty segment that would point the read boundary somewhere
/// surprising (crashing beats data corruption): a typo'd `$TMPDR` fails at load, not by
/// quietly granting `/`. Env expansion runs *first* so a leading `~` in a variable's
/// *value* is still expanded (`MYDIR=~/data; allow_paths=["$MYDIR"]` works), and so the
/// tilde step still resolves `~` through an `OsString` `$HOME` — a non-UTF-8 home survives
/// *via the `~` form* (`expand_tilde` uses `var_os`); `$HOME` spelled out goes through
/// UTF-8 `std::env::var`, so on the rare non-UTF-8 home, write `~`. This is
/// what lets a user write the environment-portable `allow_paths = ["$TMPDIR"]` /
/// `["$XDG_RUNTIME_DIR/scratch"]` instead of hardcoding a host-specific `/tmp`. `$$` writes
/// a literal `$`; a stray `$` that begins no reference is a loud error rather than a silent
/// literal (a typo in a boundary path is worth catching), as is a set-but-empty or
/// non-UTF-8 variable — an empty segment must never silently re-anchor the path.
fn expand_path(s: &str) -> anyhow::Result<PathBuf> {
    Ok(expand_tilde(&expand_env_vars(s)?))
}

/// Convert an [`expand_path`] result back to an owned `String` for the few fields stored
/// as text (a backend's `api_key_file`), failing **loudly** on a non-UTF-8 result rather
/// than lossily mangling it. The `~` form expands `$HOME` through `var_os`, so a non-UTF-8
/// home survives into the `PathBuf` (see `expand_path`'s note); a `to_string_lossy` there
/// would replace those bytes with U+FFFD and the key file would mis-resolve as "absent" —
/// a silent corruption. Paths kept as `PathBuf` (`root`/`allow_paths`/`user_files`) don't
/// need this; they carry the bytes intact.
fn expanded_to_utf8(p: PathBuf, what: &str) -> anyhow::Result<String> {
    p.into_os_string().into_string().map_err(|os| {
        anyhow!("{what} expanded to a non-UTF-8 path ({os:?}); write it in UTF-8")
    })
}

/// Resolve one variable lookup to its value, refusing the two set-but-unusable cases that
/// would otherwise misplace the read boundary *silently*. Pure (the lookup result is passed
/// in) so the boundary-relevant rejections are unit-testable without mutating process env.
///
/// - **Set but empty** (`Ok("")`): an empty segment re-anchors the path — `$EMPTY/scratch`
///   collapses to `/scratch`, `$EMPTY/` to `/` (the whole filesystem root). `undefined →
///   error` doesn't catch this because an empty var *is* defined, so it's refused here.
/// - **Set but non-UTF-8** (`Err(NotUnicode)`): reported accurately instead of as "not set"
///   (the variable *is* set), pointing at `~` which handles a non-UTF-8 home via `var_os`.
fn resolve_var(name: &str, full: &str, value: Result<String, std::env::VarError>) -> anyhow::Result<String> {
    use std::env::VarError;
    match value {
        Ok(v) if v.is_empty() => bail!(
            "path {full:?} references environment variable ${name}, which is set but empty \
             — an empty segment would re-anchor the path (`$EMPTY/x` → `/x`, `$EMPTY/` → \
             `/`), so it's refused"
        ),
        Ok(v) => Ok(v),
        Err(VarError::NotPresent) => bail!(
            "path {full:?} references environment variable ${name}, which is not set \
             — set it, or write the literal path"
        ),
        Err(VarError::NotUnicode(_)) => bail!(
            "path {full:?} references environment variable ${name}, which is set but not \
             valid UTF-8 — spell it `~` if it's your home dir, else give it a UTF-8 value"
        ),
    }
}

/// Substitute `$VAR` and `${VAR}` from the process environment. Undefined / empty /
/// non-UTF-8 → loud error (see [`resolve_var`]). `$$` escapes a literal `$`; a stray `$`
/// that begins no reference is also a loud error, never a silent literal — in a boundary
/// path it's a typo worth catching. `[A-Za-z_][A-Za-z0-9_]*` is the accepted name shape.
fn expand_env_vars(s: &str) -> anyhow::Result<String> {
    fn is_start(c: char) -> bool {
        c.is_ascii_alphabetic() || c == '_'
    }
    fn is_continue(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
    fn lookup(out: &mut String, name: &str, full: &str) -> anyhow::Result<()> {
        out.push_str(&resolve_var(name, full, std::env::var(name))?);
        Ok(())
    }

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // `$$` is the escape for a literal `$` — the only way to put one in a path.
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                for nc in chars.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    // Validate as we collect, so `${/tmp}` is a clear "invalid name"
                    // error rather than the misleading "not set" the env lookup would
                    // give — and so both reference forms agree on what a name is. (The
                    // braced form *is* an explicit expansion request, so a bad name is an
                    // error here, unlike a bare `$1` which is simply not a reference.)
                    let ok = if name.is_empty() { is_start(nc) } else { is_continue(nc) };
                    if !ok {
                        bail!(
                            "path {s:?} has an invalid character {nc:?} in a ${{...}} \
                             reference — names are [A-Za-z_][A-Za-z0-9_]*"
                        );
                    }
                    name.push(nc);
                }
                if !closed {
                    bail!("path {s:?} has an unterminated ${{...}} reference");
                }
                if name.is_empty() {
                    bail!("path {s:?} has an empty ${{}} reference");
                }
                lookup(&mut out, &name, s)?;
            }
            Some(nc) if is_start(nc) => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if is_continue(nc) {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                lookup(&mut out, &name, s)?;
            }
            // A `$` that begins no reference and isn't escaped is a loud error, not a
            // silent literal — a stray `$` in a boundary path (`$1`, `$ `, a mistyped
            // `${...}`, a trailing `$`) is a typo, and keeping it silently would mask it.
            _ => bail!(
                "path {s:?} has a stray '$' — write `$$` for a literal '$', or \
                 `$NAME` / `${{NAME}}` to expand a variable"
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- expand_tilde ---------------------------------------------------------

    /// `expand_tilde` must expand only a leading `~` / `~/...` to `$HOME`, and leave
    /// everything else — `~user`, a mid-path `~`, absolute and relative paths —
    /// verbatim. The non-expansion of `~user` is a security property (no getpwnam from
    /// a path that feeds containment), so it gets its own teeth here.
    #[test]
    fn expand_tilde_only_touches_a_leading_home_tilde() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let home = home.trim_end_matches('/');

        assert_eq!(expand_tilde("~/src"), PathBuf::from(format!("{home}/src")));
        assert_eq!(expand_tilde("~"), PathBuf::from(home));

        // Passthrough cases — left exactly as written.
        for verbatim in ["~user", "~user/x", "/abs/~/x", "rel/path", "/data/fixtures"] {
            assert_eq!(
                expand_tilde(verbatim),
                PathBuf::from(verbatim),
                "{verbatim:?} must pass through unchanged"
            );
        }
    }

    // --- expand_env_vars / expand_path ---------------------------------------

    /// `$VAR` and `${VAR}` resolve from the environment; both spellings agree. Uses
    /// `HOME` (always set in the test env) as the live variable so the test doesn't
    /// mutate process env — which would race the parallel suite.
    #[test]
    fn expand_env_vars_resolves_both_spellings() {
        let home = std::env::var("HOME").expect("HOME set in test env");

        assert_eq!(expand_env_vars("$HOME/src").unwrap(), format!("{home}/src"));
        assert_eq!(expand_env_vars("${HOME}/src").unwrap(), format!("{home}/src"));
        // A reference mid-path, and back-to-back text.
        assert_eq!(expand_env_vars("/x/${HOME}y").unwrap(), format!("/x/{home}y"));
        // Adjacent references concatenate; the second `$` ends the first name and starts
        // a new reference (`is_continue` ⊇ `is_start`, so the name loop is well-behaved).
        assert_eq!(expand_env_vars("$HOME$HOME").unwrap(), format!("{home}{home}"));
        assert_eq!(expand_env_vars("${HOME}${HOME}").unwrap(), format!("{home}{home}"));
    }

    /// An undefined variable is a loud error, not a silent empty segment — the property
    /// that keeps a typo'd `$TMPDR` from quietly widening the read boundary. Covers the
    /// bare form, the braced form, and the malformed `${` / `${}` shapes.
    #[test]
    fn expand_env_vars_undefined_or_malformed_is_loud() {
        // A name we can be confident is unset in any sane test environment.
        let unset = "KAIBO_DEFINITELY_UNSET_VAR_9Q";
        assert!(std::env::var(unset).is_err(), "test precondition: {unset} unset");

        for bad in [
            format!("${unset}"),       // bare form, undefined
            format!("${{{unset}}}"),   // braced form, undefined
            "${UNTERMINATED".to_string(),
            "${}".to_string(),
            // The braced form is an explicit expansion request, so an invalid name is a
            // loud parse error (not the misleading "not set" the env lookup would give).
            "${1bad}".to_string(),     // can't start with a digit
            "${A/B}".to_string(),      // `/` is not a name char
            // A stray `$` that begins no reference is a typo in a boundary path, refused
            // rather than kept as a silent literal. Write `$$` for a real literal `$`.
            "/a/$ b".to_string(),      // `$` followed by a space
            "/cost/$100".to_string(),  // `$` followed by a digit
            "trailing$".to_string(),   // `$` at end of string
        ] {
            assert!(
                expand_env_vars(&bad).is_err(),
                "{bad:?} must fail loudly, not expand to a silent gap"
            );
        }
    }

    /// `$$` is the escape for a literal `$` — the only way to put one in a path — and a
    /// path with no `$` is untouched. (A stray, unescaped `$` errors; that's covered by
    /// `expand_env_vars_undefined_or_malformed_is_loud`.)
    #[test]
    fn expand_env_vars_double_dollar_escapes_a_literal_dollar() {
        assert_eq!(expand_env_vars("a$$b").unwrap(), "a$b");
        assert_eq!(expand_env_vars("/cost/$$100").unwrap(), "/cost/$100");
        assert_eq!(expand_env_vars("$$").unwrap(), "$");
        assert_eq!(expand_env_vars("/data/fixtures").unwrap(), "/data/fixtures");
    }

    /// `resolve_var` refuses the two set-but-unusable cases that would silently misplace
    /// the boundary — empty value (`$EMPTY/` → `/`) and non-UTF-8 — while a normal value
    /// passes. Pure, so this has teeth without mutating process env (which would race the
    /// parallel suite, and is `unsafe` besides).
    #[test]
    fn resolve_var_refuses_empty_and_non_utf8() {
        use std::env::VarError;
        use std::ffi::OsString;

        assert_eq!(resolve_var("X", "$X", Ok("/tmp/scratch".into())).unwrap(), "/tmp/scratch");
        assert!(
            resolve_var("X", "$X", Ok(String::new())).is_err(),
            "a set-but-empty variable must be refused — an empty segment re-anchors the path"
        );
        assert!(resolve_var("X", "$X", Err(VarError::NotPresent)).is_err());
        assert!(
            resolve_var("X", "$X", Err(VarError::NotUnicode(OsString::from("x")))).is_err(),
            "a set-but-non-UTF-8 variable must be refused (not reported as 'not set')"
        );
    }

    /// `expand_path` composes env expansion *then* tilde: `$HOME` and `~` reach the same
    /// place, and `~` still works when no variable is present (the boundary knobs depend
    /// on both forms resolving).
    #[test]
    fn expand_path_does_env_then_tilde() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let home = home.trim_end_matches('/');

        assert_eq!(expand_path("~/src").unwrap(), PathBuf::from(format!("{home}/src")));
        assert_eq!(expand_path("$HOME/src").unwrap(), PathBuf::from(format!("{home}/src")));
        assert_eq!(expand_path("/data/fixtures").unwrap(), PathBuf::from("/data/fixtures"));
        // Undefined variable propagates as an error through expand_path, not a panic.
        assert!(expand_path("$KAIBO_DEFINITELY_UNSET_VAR_9Q/x").is_err());
    }

    /// The expanded-path → `String` conversion rejects a non-UTF-8 result loudly rather
    /// than lossily mangling it (a non-UTF-8 `$HOME` reached via a `~` path would become
    /// U+FFFD bytes and mis-resolve at key time — a silent corruption we refuse). A normal
    /// path round-trips byte-for-byte.
    #[cfg(unix)]
    #[test]
    fn expanded_to_utf8_rejects_non_utf8_loudly() {
        use std::os::unix::ffi::OsStringExt;
        let bad = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xFF, 0xFE]));
        assert!(
            expanded_to_utf8(bad, "test").is_err(),
            "a non-UTF-8 expanded path must fail loudly, not lossily convert"
        );
        assert_eq!(
            expanded_to_utf8(PathBuf::from("/home/u/.key"), "test").unwrap(),
            "/home/u/.key"
        );
    }

    // --- uniform $VAR expansion: [context] user_files + backend api_key_file ----
    //
    // The boundary knobs (`root`/`allow_paths`) already take `$VAR`; these two paths
    // lagged on tilde-only. One uniform rule means a user never has to remember "env
    // vars work here but not there." Uses `$HOME` (always set) so no test mutates env.

    /// `[context] user_files` expands `$VAR` *and* `~`, not tilde alone — so a portable
    /// `$XDG_CONFIG_HOME/notes.md` resolves rather than landing as a literal token.
    #[test]
    fn user_files_expand_env_vars_not_just_tilde() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let home = home.trim_end_matches('/');
        let cfg =
            Config::from_toml_str("[context]\nuser_files = [\"$HOME/notes.md\", \"~/other.md\"]")
                .unwrap();
        assert_eq!(
            cfg.context.user_files,
            vec![
                PathBuf::from(format!("{home}/notes.md")),
                PathBuf::from(format!("{home}/other.md")),
            ]
        );
    }

    /// A backend's `api_key_file` expands `$VAR` too, resolved once at load (so the
    /// use-sites stay infallible) and stored expanded.
    #[test]
    fn api_key_file_expands_env_vars_not_just_tilde() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        let home = home.trim_end_matches('/');
        let cfg =
            Config::from_toml_str("[backends.anthropic]\napi_key_file = \"$HOME/.akey\"").unwrap();
        let b = cfg.resolve_backend("anthropic").unwrap();
        assert_eq!(b.api_key_file.as_deref(), Some(format!("{home}/.akey").as_str()));
    }

    /// An undefined variable in either path is a loud load error — never a silent gap
    /// that would point a key read or a context file at the wrong place.
    #[test]
    fn undefined_var_in_user_files_or_api_key_file_is_loud_at_load() {
        let unset = "KAIBO_DEFINITELY_UNSET_VAR_9Q";
        assert!(std::env::var(unset).is_err(), "test precondition: {unset} unset");
        assert!(
            Config::from_toml_str(&format!(
                "[context]\nuser_files = [\"${unset}/notes.md\"]"
            ))
            .is_err(),
            "undefined var in user_files must fail at load"
        );
        assert!(
            Config::from_toml_str(&format!(
                "[backends.anthropic]\napi_key_file = \"${unset}/k\""
            ))
            .is_err(),
            "undefined var in api_key_file must fail at load"
        );
    }

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

    /// All four interactive built-in casts exist, each single-backend with
    /// explorer+synth, and none has a synth on an offline lane.
    #[test]
    fn all_four_builtin_casts_resolve() {
        let cfg = Config::builtin();
        for name in ["anthropic", "deepseek", "gemini", "openai-local"] {
            let cast = cfg.resolve_cast(name).unwrap();
            assert_eq!(
                cast.synth_lane(),
                None,
                "interactive built-in {name:?} must have an interactive synth"
            );
            for role in [ModelRole::Explorer, ModelRole::Synth] {
                let slot = cast.require_slot(role).unwrap();
                assert_eq!(slot.backend, name, "built-in casts are single-backend");
            }
        }
    }

    /// The built-in batch casts' synth slots positively declare `lane = Batch`, carry
    /// **synth only** (batch is a toolless oneshot — an explorer slot would be dead
    /// weight), and synth a batch-capable backend. Pins the lane's shape so an
    /// accidental explorer slot or a dropped lane fails here.
    #[test]
    fn builtin_batch_casts_are_synth_only_and_flagged() {
        let cfg = Config::builtin();
        for (name, backend) in [("gemini-batch", "gemini"), ("anthropic-batch", "anthropic")] {
            let cast = cfg.resolve_cast(name).unwrap();
            assert_eq!(
                cast.synth_lane(),
                Some(Lane::Batch),
                "{name:?} must declare its synth slot's lane = batch"
            );
            assert!(
                cast.slot(ModelRole::Explorer).is_none(),
                "{name:?} is a batch cast — toolless, so it carries no explorer slot"
            );
            let synth = cast.require_slot(ModelRole::Synth).unwrap();
            assert_eq!(synth.backend, backend, "{name:?} synths its named backend");
            assert!(
                crate::batch::batch_supported(cfg.backends[&synth.backend].kind),
                "{name:?} must synth a batch-capable backend"
            );
        }
    }

    /// A `batch = true` cast whose synth backend has no batch API is a misdeclaration —
    /// caught loudly at config load, not as a 400 at submit time. The message names the
    /// cast and the live batch-capable set.
    #[test]
    fn batch_cast_on_non_batch_backend_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.bad]
            batch = true
            synth = "deepseek/deepseek-v4-pro"
            "#,
        )
        .expect_err("a batch cast on a non-batch backend must not load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bad") && msg.contains("batch"),
            "load error must name the cast and the batch problem, got: {msg}"
        );
    }

    /// A `batch = true` cast with no synth slot can't run `batch_submit` (which runs the
    /// synth model), so it's refused at load rather than failing cryptically on submit.
    #[test]
    fn batch_cast_without_synth_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.bad]
            batch = true
            explorer = "gemini/gemini-flash-lite-latest"
            "#,
        )
        .expect_err("a batch cast with no synth slot must not load");
        assert!(
            format!("{err:#}").contains("synth"),
            "load error must name the missing synth slot"
        );
    }

    /// `batch` round-trips from `[casts.<name>]`: a fresh cast defaults interactive, an
    /// explicit `batch = true` (on a batch-capable synth) sets the synth slot's
    /// `lane = Batch`, and it's sticky over a built-in — retuning `gemini-batch`'s model
    /// (a bare re-declaration, no `lane` repeated) leaves the synth slot batch.
    #[test]
    fn batch_flag_parses_defaults_false_and_is_sticky() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.fresh]
            synth = "anthropic/claude-sonnet-4-6"

            [casts.declared]
            batch = true
            synth = "anthropic/claude-opus-4-8"

            [casts.gemini-batch]
            synth = "gemini/gemini-3-pro-preview"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_cast("fresh").unwrap().synth_lane(),
            None,
            "absent batch ⇒ interactive synth"
        );
        assert_eq!(
            cfg.resolve_cast("declared").unwrap().synth_lane(),
            Some(Lane::Batch),
            "batch = true ⇒ synth lane = batch"
        );
        let gb = cfg.resolve_cast("gemini-batch").unwrap();
        assert_eq!(
            gb.synth_lane(),
            Some(Lane::Batch),
            "retuning a built-in batch cast's synth id leaves its lane batch (sticky)"
        );
        assert_eq!(
            gb.require_slot(ModelRole::Synth).unwrap().id,
            "gemini-3-pro-preview",
            "the file override reached the synth slot"
        );
    }

    /// A slot table's `lane = "direct"` parses to `Some(Lane::Direct)` — the offline,
    /// non-batch lane for a big local model kaibo runs itself.
    #[test]
    fn slot_lane_direct_parses() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.mydirect]
            synth = { backend = "openai-local", id = "big-local-model", lane = "direct" }
            "#,
        )
        .unwrap();
        let cast = cfg.resolve_cast("mydirect").unwrap();
        assert_eq!(cast.synth_lane(), Some(Lane::Direct));
    }

    /// A slot table's `lane = "batch"` parses and validates cleanly on a batch-capable
    /// backend — the table-form spelling of the same lane the `batch = true` sugar sets.
    #[test]
    fn slot_lane_batch_parses_and_validates() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.mybatch]
            synth = { backend = "gemini", id = "gemini-pro-latest", lane = "batch" }
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_cast("mybatch").unwrap().synth_lane(),
            Some(Lane::Batch)
        );
    }

    /// `batch = true` conflicting with an explicit, different synth `lane` is a loud
    /// load error naming the cast and both lanes — silently picking one would run the
    /// wrong lane's tool against the operator's actual intent.
    #[test]
    fn batch_sugar_conflicting_with_explicit_direct_lane_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.bad]
            batch = true
            synth = { backend = "openai-local", id = "m", lane = "direct" }
            "#,
        )
        .expect_err("batch = true conflicting with an explicit lane = direct must not load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bad") && msg.contains("batch") && msg.contains("direct"),
            "load error must name the cast and both conflicting lanes, got: {msg}"
        );
    }

    /// A cast may pair an interactive explorer with an offline (`batch`) synth — the
    /// `deliberate` shape this reshape exists to enable. Dropped the old "batch casts
    /// are synth-only" constraint at validation: the built-in batch casts stay
    /// synth-only by construction, but a custom cast may add an explorer.
    #[test]
    fn interactive_explorer_with_batch_synth_is_legal() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.mydeliberate]
            explorer = "anthropic/claude-haiku-4-5"
            synth = { backend = "gemini", id = "gemini-pro-latest", lane = "batch" }
            "#,
        )
        .unwrap();
        let cast = cfg.resolve_cast("mydeliberate").unwrap();
        assert_eq!(cast.synth_lane(), Some(Lane::Batch));
        assert!(cast.slot(ModelRole::Explorer).is_some());
    }

    /// A `lane` on an EXPLORER slot is a loud load error: the explorer always runs
    /// interactively, so declaring it offline is a misdeclaration, not a valid shape.
    #[test]
    fn lane_on_explorer_slot_is_a_load_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.bad]
            explorer = { backend = "gemini", id = "m", lane = "batch" }
            synth = "gemini/gemini-pro-latest"
            "#,
        )
        .expect_err("a lane on the explorer slot must not load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bad") && msg.contains("explorer"),
            "load error must name the cast and the explorer slot, got: {msg}"
        );
    }

    /// A bad `lane` value is a clear parse error naming the valid spellings —
    /// same discipline as `thinking_style`.
    #[test]
    fn bad_lane_value_is_a_clear_parse_error() {
        let err = Config::from_toml_str(
            r#"
            [casts.bad]
            synth = { backend = "gemini", id = "m", lane = "sideways" }
            "#,
        )
        .expect_err("an unknown lane value must not load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sideways") && msg.contains("batch") && msg.contains("direct"),
            "parse error must name the bad value and the valid ones, got: {msg}"
        );
    }

    // --- usability: the unconfigured-install signal ---------------------------
    //
    // These point every keyed backend's `api_key_file` at a path that cannot exist,
    // so the verdict depends ONLY on the injected env lookup — never on whether the
    // test machine happens to have ~/.anthropic-key.txt. (Amy keeps real key files;
    // a naive built-in test would pass on her box and fail in CI, or vice versa.)
    const NO_KEY_FILES: &str = r#"
        [backends.anthropic]
        api_key_file = "/nonexistent-kaibo-test/anthropic"
        [backends.deepseek]
        api_key_file = "/nonexistent-kaibo-test/deepseek"
        [backends.gemini]
        api_key_file = "/nonexistent-kaibo-test/gemini"
        [backends.openai-local]
        api_key_file = "/nonexistent-kaibo-test/openai"
    "#;

    /// No env key + no key file on a *keyed* backend ⇒ Unconfigured (the fresh
    /// install). This is the state that must light up setup guidance.
    #[test]
    fn cast_usability_unconfigured_when_keyed_backend_has_no_key() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[casts.t]\nexplorer=\"anthropic/m1\"\nsynth=\"anthropic/m2\""
        ))
        .unwrap();
        let cast = cfg.resolve_cast("t").unwrap();
        assert_eq!(
            cfg.cast_usability(cast, |_| None),
            CastUsability::Unconfigured
        );
    }

    /// The same config flips to Ready the moment the env var carries a key — so an
    /// env-only setup (no config file) is never nagged.
    #[test]
    fn cast_usability_ready_when_env_key_present() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[casts.t]\nexplorer=\"anthropic/m1\"\nsynth=\"anthropic/m2\""
        ))
        .unwrap();
        let cast = cfg.resolve_cast("t").unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string());
        assert_eq!(cfg.cast_usability(cast, env), CastUsability::Ready);
        // A blank env value doesn't count as present — falls through to the file.
        let blank = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "   ".to_string());
        assert_eq!(cfg.cast_usability(cast, blank), CastUsability::Unconfigured);
    }

    /// A keyless (`key_optional`) backend with no key reads as LocalUnverified, not
    /// Unconfigured — we don't probe localhost, and the user may have chosen local.
    #[test]
    fn cast_usability_local_unverified_for_keyless_placeholder() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[casts.t]\nexplorer=\"openai-local/m1\"\nsynth=\"openai-local/m2\""
        ))
        .unwrap();
        let cast = cfg.resolve_cast("t").unwrap();
        assert_eq!(
            cfg.cast_usability(cast, |_| None),
            CastUsability::LocalUnverified
        );
    }

    /// Partial setup is broken: one slot keyed-and-present, the other keyed-and-missing
    /// ⇒ Unconfigured. (Amy's call — a half-configured cast can't answer.)
    #[test]
    fn cast_usability_unconfigured_if_any_slot_missing() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[casts.t]\nexplorer=\"anthropic/m1\"\nsynth=\"deepseek/m2\""
        ))
        .unwrap();
        let cast = cfg.resolve_cast("t").unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string());
        assert_eq!(cfg.cast_usability(cast, env), CastUsability::Unconfigured);
    }

    /// Present + placeholder (no Missing) ⇒ LocalUnverified: a deliberate keyed-explorer
    /// + local-synth chimera is configured, not a fresh install.
    #[test]
    fn cast_usability_local_unverified_when_mixing_present_and_placeholder() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[casts.t]\nexplorer=\"anthropic/m1\"\nsynth=\"openai-local/m2\""
        ))
        .unwrap();
        let cast = cfg.resolve_cast("t").unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string());
        assert_eq!(
            cfg.cast_usability(cast, env),
            CastUsability::LocalUnverified
        );
    }

    /// `is_default_cast` matches the canonical default *and* an alias default — the
    /// roster's `(default)` tag depends on it, and an operator may set `server.cast` to
    /// an alias. Compares against the resolved name, not the raw string.
    #[test]
    fn is_default_cast_matches_through_an_alias() {
        let mut cfg = Config::builtin(); // `claude` is a built-in alias for `anthropic`
        cfg.default_cast = "claude".to_string();
        assert!(
            cfg.is_default_cast("anthropic"),
            "an alias default must match its canonical cast"
        );
        assert!(
            !cfg.is_default_cast("claude"),
            "the canonical name is what usable_casts yields and what we compare"
        );
        assert!(
            !cfg.is_default_cast("deepseek"),
            "a non-default cast must not be tagged default"
        );
    }

    /// `default_cast_usability` reads the *default* cast, so it tracks `server.cast`.
    #[test]
    fn default_cast_usability_follows_the_default_cast() {
        let cfg = Config::from_toml_str(&format!(
            "{NO_KEY_FILES}\n[server]\ncast=\"t\"\n\
             [casts.t]\nexplorer=\"anthropic/m1\"\nsynth=\"anthropic/m2\""
        ))
        .unwrap();
        assert_eq!(cfg.default_cast, "t");
        assert_eq!(
            cfg.default_cast_usability(|_| None),
            CastUsability::Unconfigured
        );
    }

    /// `usable_casts` keeps only the casts that can reach a model — filtering out the
    /// `Unconfigured` ones — and reports each survivor's state so the handshake can tag
    /// a local one. With only an Anthropic key in the env, the keyed built-ins that lack
    /// keys (deepseek, gemini) drop out; anthropic and the Anthropic-synth `anthropic-batch`
    /// are Ready and the keyless `openai-local` is LocalUnverified. (The roster spans both lanes;
    /// the per-tool `cast` enums partition it by `batch`.) This is the source of the
    /// truthful "## Casts" handshake list.
    #[test]
    fn usable_casts_filters_unconfigured_and_reports_state() {
        let cfg = Config::from_toml_str(NO_KEY_FILES).unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string());
        let usable = cfg.usable_casts(env);
        assert_eq!(
            usable,
            vec![
                ("anthropic".to_string(), CastUsability::Ready),
                ("anthropic-batch".to_string(), CastUsability::Ready),
                ("openai-local".to_string(), CastUsability::LocalUnverified),
            ],
            "only keyed-and-present + keyless casts survive, with their state"
        );
        // With no key at all, the only survivor is the keyless local cast.
        let none = cfg.usable_casts(|_| None);
        assert_eq!(
            none,
            vec![("openai-local".to_string(), CastUsability::LocalUnverified)],
            "no env key ⇒ only the keyless local cast is usable"
        );
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
            assert_eq!(cfg.resolve_cast(a).unwrap().name, "openai-local");
        }
        // Backend level.
        assert_eq!(cfg.resolve_backend("claude").unwrap().name, "anthropic");
        assert_eq!(cfg.resolve_backend("google").unwrap().name, "gemini");
        assert_eq!(cfg.resolve_backend("local").unwrap().name, "openai-local");
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
            [casts.chimera]
            explorer = "deepseek/deepseek-v4-flash"
            synth = { backend = "claude", id = "claude-opus-4-8", effort = "max" }
            "#,
        )
        .unwrap();
        let cast = cfg.resolve_cast("chimera").unwrap();
        let e = cast.require_slot(ModelRole::Explorer).unwrap();
        let s = cast.require_slot(ModelRole::Synth).unwrap();
        assert_eq!(e.qualified(), "deepseek/deepseek-v4-flash");
        assert_eq!(s.qualified(), "anthropic/claude-opus-4-8");
        assert_eq!(s.effort.as_deref(), Some("max"));
        // Caps classify on the slot's backend kind: DeepSeek is blind, Anthropic sees.
        assert!(!cfg.slot_caps(e).unwrap().vision);
        assert!(cfg.slot_caps(s).unwrap().vision);
        // A vision pin on a generic openai slot overrides the classifier.
        let pinned = ModelSlot {
            vision: Some(true),
            ..ModelSlot::bare("openai-local", "llava")
        };
        assert!(cfg.slot_caps(&pinned).unwrap().vision);
    }

    /// A model id with an inner slash (HuggingFace style) survives the string
    /// form: only the FIRST `/` splits backend from id.
    #[test]
    fn slot_ref_splits_on_the_first_slash_only() {
        let (backend, id) = parse_slot_ref("openai-local/Qwen/Qwen3-32B").unwrap();
        assert_eq!(backend, "openai-local");
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

    /// A backend name containing '/' is refused at load. Both the slot ref
    /// (`"backend/model-id"`) and the batch handle (`"backend/provider-id"`) split on
    /// the first slash and trust the prefix to be slash-free, so a slash-bearing name
    /// would silently mis-route. Enforce the invariant the parsers depend on.
    #[test]
    fn backend_name_with_slash_is_refused() {
        let err = Config::from_toml_str(
            "[backends.\"foo/bar\"]\nkind = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("may not contain '/'"), "got: {err}");
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
            synth = { backend = "openai-local", id = "m", max_tokens = 1000, thinking_budget = 2000 }
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

    /// The table slot form trims `backend`/`id` just like the `"backend/id"` string
    /// form (`parse_slot_ref`): identical intent must not hinge on the spelling. A
    /// padded `backend = " anthropic "` would otherwise miss the backend map and
    /// surface as a baffling "unknown backend" — same class as a padded ref.
    #[test]
    fn a_table_slot_trims_backend_and_id_like_the_ref_form() {
        let cfg = Config::from_toml_str(
            r#"
            [casts.x]
            synth = { backend = " anthropic ", id = "  claude-opus-4-8  " }
            "#,
        )
        .unwrap();
        let slot = cfg
            .resolve_cast("x")
            .unwrap()
            .require_slot(ModelRole::Synth)
            .unwrap();
        assert_eq!(slot.backend, "anthropic");
        assert_eq!(slot.id, "claude-opus-4-8");
    }

    // --- scratch budget ---------------------------------------------------------

    /// `[sandbox].scratch_limit_bytes` overrides the default cap on the `/` scratch
    /// MemoryFs; absent, the generous built-in default stands.
    #[test]
    fn scratch_limit_bytes_overrides_the_default() {
        let cfg = Config::from_toml_str("[sandbox]\nscratch_limit_bytes = 1024\n").unwrap();
        assert_eq!(cfg.sandbox.scratch_limit_bytes, 1024);
        // Default when unset.
        let cfg = Config::builtin();
        assert_eq!(
            cfg.sandbox.scratch_limit_bytes,
            crate::sandbox::DEFAULT_SCRATCH_LIMIT_BYTES
        );
    }

    /// A zero scratch budget refuses every scratch write — never a real intent, so
    /// it's a loud load error, not a baffling StorageFull on the first redirect.
    #[test]
    fn a_zero_scratch_limit_is_a_load_error() {
        let err = Config::from_toml_str("[sandbox]\nscratch_limit_bytes = 0\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("scratch_limit_bytes"), "got: {err}");
        assert!(err.contains("> 0"), "got: {err}");
    }

    /// `KAIBO_SCRATCH_LIMIT_BYTES` folds in over the file (env > file).
    #[test]
    fn env_scratch_limit_bytes_overrides_the_file() {
        let cfg = Config::load_with(None, None, |k| {
            (k == "KAIBO_SCRATCH_LIMIT_BYTES").then(|| "2048".to_string())
        })
        .unwrap();
        assert_eq!(cfg.sandbox.scratch_limit_bytes, 2048);
    }

    // --- ignore policy ([kaish.ignore]) -----------------------------------------

    /// An absent `[kaish]` stanza reproduces the kernel's agent ignore default exactly:
    /// `.gitignore` loaded, built-in defaults on, auto-gitignore on, enforced scope.
    /// So omitting it keeps today's behavior.
    #[test]
    fn ignore_default_matches_agent() {
        let cfg = Config::builtin();
        let ig = &cfg.sandbox.ignore;
        assert_eq!(ig.files(), &[".gitignore"]);
        assert!(ig.use_defaults());
        assert!(ig.auto_gitignore());
        assert!(!ig.use_global_gitignore());
        assert_eq!(ig.scope(), IgnoreScope::Enforced);
    }

    /// `[kaish.ignore].files` is the explicit list — extra ignore files reach the
    /// kernel, and the order is preserved (precedence is later-wins).
    #[test]
    fn ignore_files_are_configurable() {
        let cfg =
            Config::from_toml_str("[kaish.ignore]\nfiles = [\".gitignore\", \".claudeignore\"]\n")
                .unwrap();
        assert_eq!(
            cfg.sandbox.ignore.files(),
            &[".gitignore".to_string(), ".claudeignore".to_string()]
        );
    }

    /// A partial stanza overrides only the keys it names; the rest keep the agent
    /// default. Here `scope = "advisory"` flips scope while `files`/`defaults` stay.
    #[test]
    fn ignore_partial_stanza_overrides_only_named_keys() {
        let cfg = Config::from_toml_str(
            "[kaish.ignore]\nscope = \"advisory\"\nglobal_gitignore = true\n",
        )
        .unwrap();
        let ig = &cfg.sandbox.ignore;
        assert_eq!(ig.scope(), IgnoreScope::Advisory);
        assert!(ig.use_global_gitignore());
        // Untouched keys keep their agent defaults.
        assert_eq!(ig.files(), &[".gitignore"]);
        assert!(ig.use_defaults());
        assert!(ig.auto_gitignore());
    }

    /// An unrecognized `scope` is a loud load error, not a silent fall-back to the
    /// default — a typo here would otherwise quietly leave `find` unrestricted.
    #[test]
    fn an_unknown_ignore_scope_is_a_load_error() {
        let err = Config::from_toml_str("[kaish.ignore]\nscope = \"strict\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope"), "got: {err}");
        assert!(err.contains("enforced"), "got: {err}");
    }

    /// A typo'd key under `[kaish.ignore]` is a loud load error (deny_unknown_fields),
    /// not a silently-ignored no-op.
    #[test]
    fn an_unknown_ignore_key_is_a_load_error() {
        assert!(Config::from_toml_str("[kaish.ignore]\nfles = [\".x\"]\n").is_err());
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
