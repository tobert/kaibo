//! The `kaibo://config` resource renderer — the resolved runtime configuration
//! serialized to an annotated TOML document (allowed trees, gated tools, sandbox
//! limits, backends and casts), with the render-only `*Doc` shapes it builds from.
//! Renders key *source* metadata (env var names, key-file paths), never resolved
//! secret values.

use std::path::{Path, PathBuf};

use crate::config::{Config, Lane, ModelSlot};
use crate::consult::{ModelShape, ThinkingStyleOverride};
use crate::credentials::WireKind;

use super::ToolGating;

/// Render the `kaibo://config` TOML document. Shows the resolved runtime state —
/// allowed trees, default cast, gated tools, sandbox limits, tunable defaults,
/// each backend's kind/endpoint/key sources, and each cast's slots as
/// `"backend/id"` with *resolved* caps — so a calling model or operator sees the
/// server's current posture at a glance.
///
/// SECRET-SAFETY CONTRACT: this function renders key SOURCE metadata (env var names,
/// key file paths — the operator-configured pointers) but NEVER the resolved key
/// values. The backend struct stores sources, not secrets; this renderer reads only
/// those source fields. If Config ever gains a resolved-key cache, do not read it here.
/// Tests in this file assert the contract holds.
// The inputs are inherently many — the resolved config plus five pieces of runtime truth
// the renderer cannot reach for itself (allowed set, default root and how it was chosen,
// live worktrees, persistence, CAS mode and its backing). Bundling them into a struct
// would relocate the list, not shorten it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_config_resource(
    config: &Config,
    allowed_set: &[PathBuf],
    default_root: Option<&Path>,
    default_root_inferred: bool,
    followed_worktrees: Vec<PathBuf>,
    persistence_active: bool,
    cas_mode: crate::config::CasMode,
    cas_ephemeral_fs: Option<&'static str>,
) -> String {
    use serde::Serialize;
    use std::collections::BTreeMap;

    // Which cast-taking tools nothing can staff right now, and the cast shape each one
    // wants. Computed through the same `eligible_casts_by_tool` the router gated on, so
    // the explanation can't drift from the decision. A tool the OPERATOR turned off is
    // deliberately absent here — `[tools]` already reports that, and conflating "you
    // disabled it" with "nothing can run it" is exactly the confusion this section exists
    // to remove.
    let usable: Vec<String> = config
        .usable_casts(|k| std::env::var(k).ok())
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let unstaffable: BTreeMap<String, String> = super::eligible_casts_by_tool(config, &usable)
        .into_iter()
        .filter(|(tool, casts)| casts.is_empty() && config.tools.enabled(tool))
        .map(|(tool, _)| {
            let requirement = super::cast_requirement_for(tool)
                .expect("a tool in the eligibility map has a rule")
                .to_string();
            (tool.to_string(), requirement)
        })
        .collect();

    // Dedicated render-only shapes — plain Serialize structs that carry exactly what
    // the resource must expose and nothing more. Keeps the contract explicit.

    #[derive(Serialize)]
    struct ConfigDoc {
        /// Allowed path trees: a per-call path must be at-or-under one of these.
        allowed_paths: Vec<String>,
        /// The effective default root a call uses when it omits `path` — an explicit
        /// `--root`, or the launch cwd kaibo inferred. Absent when neither applies.
        #[serde(skip_serializing_if = "Option::is_none")]
        default_root: Option<String>,
        /// True when `default_root` was inferred from the launch cwd rather than
        /// configured explicitly. Only meaningful when `default_root` is present.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        default_root_inferred: bool,
        /// Default cast name (what a call omitting `cast` gets).
        default_cast: String,
        /// Runtime-derived state — computed at read time, not configured. Distinct
        /// from the static knobs above so a reader can tell "what kaibo discovered"
        /// from "what the operator set".
        runtime: RuntimeDoc,
        /// Which tools are currently advertised.
        tools: ToolsDoc,
        /// Read-only sandbox limits.
        sandbox: SandboxDoc,
        /// kaish kernel behavior tuning (the `[kaish]` stanza) — currently the
        /// resolved ignore policy the file-walking builtins honor.
        kaish: KaishDoc,
        /// The [defaults] tunables every slot falls back to.
        defaults: DefaultsDoc,
        /// OpenTelemetry export state (off by default). Header *names* only — a
        /// value could be a bearer token, so it's withheld like an API key.
        telemetry: TelemetryDoc,
        /// Durable-store state (on by default): whether persistence is enabled, the
        /// resolved state-db path, and whether the store is actually open right now.
        persistence: PersistenceDoc,
        /// The media CAS: the on/off knob, the runtime mode (disk / memory / off),
        /// and where disk mode writes.
        cas: CasDoc,
        /// `[artifacts]`: whether this server allows the model team to save artifacts.
        /// Off by default; a call must also pass `save_artifacts`.
        artifacts: ArtifactsDoc,
        /// alias → canonical backend name. Aliases are valid slot-ref prefixes
        /// and per-call backend overrides, so callers must be able to discover
        /// them here — built-in and file-declared both.
        backend_aliases: BTreeMap<String, String>,
        /// Backends (connections): kind, endpoint, key sources (never key values).
        backends: BTreeMap<String, BackendDoc>,
        /// alias → canonical cast name (each a valid `cast` call-param value).
        cast_aliases: BTreeMap<String, String>,
        /// Casts (compositions): slots as "backend/id" with resolved caps.
        casts: BTreeMap<String, CastDoc>,
    }

    #[derive(Serialize)]
    struct ToolsDoc {
        consult: bool,
        explore: bool,
        deliberate: bool,
        oneshot: bool,
        run_kaish: bool,
        batch: bool,
        list_models: bool,
        generate: bool,
    }

    /// Runtime-computed scope state. `follow_worktrees` echoes the knob;
    /// `followed_worktrees` is the live extra set the follow feature grants beyond
    /// `allowed_paths` right now — git worktrees of an already-allowed repo,
    /// resolved by reading git's link files. Recomputed on each read, so a worktree
    /// added mid-session shows up here without a reconnect.
    ///
    /// `advertised_tools` and `unstaffable_tools` are the observed answer to "why can't I
    /// see that tool?" — the question the staffing gate would otherwise leave an operator
    /// guessing at, since a tool no configured cast can staff is removed from the router
    /// outright rather than shipped with an empty `cast` enum. `[tools]` above says what
    /// the operator *asked for* (the `--no-<tool>` flags); these say what the server
    /// actually ended up advertising and, for each tool held back by its cast roster
    /// rather than by a flag, the cast shape that would bring it back. Keeping the two
    /// apart is the same `[runtime]` rule the worktree fields follow: what kaibo
    /// *discovered*, never what the operator *chose*.
    #[derive(Serialize)]
    struct RuntimeDoc {
        follow_worktrees: bool,
        followed_worktrees: Vec<String>,
        advertised_tools: Vec<String>,
        unstaffable_tools: BTreeMap<String, String>,
    }

    #[derive(Serialize)]
    struct SandboxDoc {
        exec_timeout_secs: u64,
        output_limit_bytes: usize,
        /// Cap on the `/` scratch MemoryFs in bytes; a write past it fails loudly.
        scratch_limit_bytes: u64,
        /// Builtins shadow-blocked beyond the structural read-only guards.
        disable_builtins: Vec<String>,
    }

    #[derive(Serialize)]
    struct KaishDoc {
        ignore: IgnoreDoc,
    }

    /// The resolved `[kaish.ignore]` policy the file-walking builtins honor.
    #[derive(Serialize)]
    struct IgnoreDoc {
        /// Ignore filenames loaded (root + ancestors), in precedence order.
        files: Vec<String>,
        /// Built-in defaults (`target/`, `node_modules/`, `.git`) applied.
        defaults: bool,
        /// Nested `.gitignore` files auto-loaded during the walk.
        auto_gitignore: bool,
        /// User's global gitignore (`core.excludesFile`) honored.
        global_gitignore: bool,
        /// `"enforced"` (all walkers incl. `find`) or `"advisory"` (polite tools only).
        scope: &'static str,
    }

    #[derive(Serialize)]
    struct DefaultsDoc {
        explorer_max_turns: usize,
        synth_max_turns: usize,
        max_tokens: u64,
        thinking_budget: u64,
        explorer_temperature: f64,
        synth_temperature: f64,
        top_p: f64,
        explorer_effort: String,
        synth_effort: String,
        thinking_style: String,
        request_timeout_secs: u64,
        call_deadline_secs: u64,
        session_capacity: usize,
        job_capacity: usize,
        inline_attach_budget: usize,
        max_attachments: usize,
    }

    /// Telemetry as resolved. SECRET-SAFETY: `header_names` lists the keys of any
    /// configured export headers but never their values — an Authorization value is
    /// a secret, same as an API key. The operator set the names; surfacing those is
    /// the discoverability the resource promises.
    #[derive(Serialize)]
    struct TelemetryDoc {
        enabled: bool,
        endpoint: String,
        /// Whether kaibo's own `tracing` events export alongside the span tree.
        logs: bool,
        /// Where those records go, **as resolved** — the explicit `logs_endpoint` or
        /// the one derived from `endpoint`. Absent when `logs` is off. Resolved
        /// rather than echoed because the derivation is the part a reader can't do
        /// in their head from the file.
        #[serde(skip_serializing_if = "Option::is_none")]
        logs_endpoint: Option<String>,
        timeout_secs: u64,
        service_name: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        header_names: Vec<String>,
    }

    /// Persistence as resolved. `path` is the state db kaibo would open (absent only
    /// when disabled and no default resolved). `active` is runtime truth — the store is
    /// open now. With `enabled`, a failed open is a loud startup error, so a running
    /// server shows `active == enabled`; surfaced so a reader confirms durability is live,
    /// not merely requested. No secrets: a db path is a path, like `default_root`.
    #[derive(Serialize)]
    struct PersistenceDoc {
        enabled: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        active: bool,
    }

    /// The media CAS as resolved. `mode` is runtime truth (`"disk"` / `"memory"` /
    /// `"off"`) from the store the handler actually holds — `"memory"` is the loud
    /// degraded state (artifacts do not survive a restart; startup already warned).
    /// `dir` is the configured directory whether or not the current mode uses it, so
    /// an operator diagnosing `"memory"` still sees where disk mode would write.
    #[derive(Serialize)]
    struct CasDoc {
        enabled: bool,
        mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
        /// The ephemeral filesystem the store is sitting on, when the startup probe
        /// found one — absent otherwise, so a normal install reads exactly as before.
        ///
        /// Present here and not only in the startup log because startup log is the thing
        /// an operator scrolls past, or never sees at all when a client launched kaibo
        /// for them. `mode = "disk"` claims durability; this is the line that qualifies
        /// it, at a place they can query after the fact.
        #[serde(skip_serializing_if = "Option::is_none")]
        backing: Option<String>,
    }

    /// `[artifacts]` as resolved: may the inner model team save what it writes?
    ///
    /// Rendered even though it is one bool, because it is the only tool switch whose
    /// default is OFF and the only one a *call* also has to ask for. Without it here, a
    /// caller whose `save_artifacts` was refused has no way to see which of the two keys
    /// this server is missing.
    #[derive(Serialize)]
    struct ArtifactsDoc {
        enabled: bool,
    }

    #[derive(Serialize)]
    struct BackendDoc {
        kind: String,
        /// Resolved endpoint for openai-kind backends (explicit base_url, else
        /// OPENAI_BASE_URL, else the built-in default) — the "resolved runtime
        /// state" promise. The raw configured value for anthropic- and gemini-kind
        /// backends, when set (an Anthropic/Gemini-API-compatible gateway/proxy);
        /// absent otherwise. Every other kind has a fixed endpoint baked into rig.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        /// Env var name whose value is the API key (checked first). The NAME, not
        /// the value — the operator configured this pointer.
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key_env: Option<String>,
        /// Key file path, resolved (`$VAR`/`~` expanded once at config load), so this
        /// shows the absolute path kaibo actually reads — consistent with how
        /// `allowed_paths`/`default_root` render resolved here. Used when the env var is
        /// unset/blank. The PATH, not its contents.
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key_file: Option<String>,
        /// True when a missing key falls back to a placeholder (keyless endpoint).
        key_optional: bool,
        request_timeout_secs: u64,
        /// OpenRouter only: the upstream-host data policy this backend requests
        /// (`"deny"` routes only to no-collection hosts — the default; `"allow"`
        /// is the explicit opt-in). Rendered so the privacy posture is visible,
        /// absent on every other kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        data_collection: Option<&'static str>,
        /// openai kind only: the *resolved* interactive request wire —
        /// `"responses"` or `"chat"` (see [`crate::config::Backend::uses_responses_wire`]).
        /// An explicit `wire` config value renders as itself; unset renders the
        /// endpoint-exact heuristic's answer, so the effective shape is always
        /// visible even when nothing was configured. Absent on every other kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        wire: Option<&'static str>,
    }

    /// One cast slot: the `"backend/id"` ref plus its *resolved* capabilities
    /// (slot pin applied, else the classifier on the slot's backend kind) and any
    /// per-slot tunable overrides actually set — the effective runtime state.
    #[derive(Serialize)]
    struct SlotDoc {
        model: String,
        vision: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_budget: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_style: Option<String>,
        /// The per-model system-prompt override, verbatim (not a secret — it's the
        /// operator's own framing). Absent when unset.
        #[serde(skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
        /// How this slot runs — `"batch"` or `"direct"`; absent (the common case)
        /// means interactive. Only ever set on a synth slot (load-validated).
        #[serde(skip_serializing_if = "Option::is_none")]
        lane: Option<&'static str>,
        /// Tunables in effect for this slot that its resolved request shape will never
        /// send — the honest no-op flag. A `thinking_budget` on an effort-driven or
        /// toggle-less model, an `effort` on a budget/toggle-less model, a `temperature`
        /// an Anthropic slot drops under thinking: each load-validates and would otherwise
        /// render as if effective. Absent when every knob in play has a sink.
        ///
        /// `effort` is judged on the **effective** value, not just a per-slot override —
        /// a `[defaults]`/env effort lands on every cast, so a per-slot-only check missed
        /// the case that bites hardest — and it is judged by
        /// [`Config::effort_disposition`](crate::config::Config::effort_disposition), the
        /// single implementation of that policy, which the startup warning reads too.
        /// Two copies of this rule diverged once already; there is now only one.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        inert_tunables: Vec<&'static str>,
    }

    /// A cast's role table, keyed by role. Only configured roles appear.
    type CastDoc = BTreeMap<&'static str, SlotDoc>;

    let backends: BTreeMap<String, BackendDoc> = config
        .backends
        .iter()
        .map(|(name, b)| {
            // Exhaustive destructure — any new Backend field is a compile error
            // here, forcing an explicit render-or-skip decision (including the
            // secret-safety review for any field that might resolve a key value).
            let crate::config::Backend {
                name: _,
                kind,
                base_url,
                api_key_env,
                api_key_file,
                key_optional,
                request_timeout,
                data_collection,
                wire: _,
            } = b;
            let rendered_base_url = if *kind == crate::credentials::ProviderKind::Openai {
                Some(b.resolved_base_url())
            } else {
                base_url.clone()
            };
            let doc = BackendDoc {
                kind: kind.canonical_name().to_string(),
                base_url: rendered_base_url,
                // KEY SOURCE ONLY — env var name or file path, never the value.
                api_key_env: api_key_env.clone(),
                api_key_file: api_key_file.clone(),
                key_optional: *key_optional,
                request_timeout_secs: request_timeout.as_secs(),
                data_collection: (*kind == crate::credentials::ProviderKind::OpenRouter).then_some(
                    match data_collection {
                        crate::config::DataCollection::Deny => "deny",
                        crate::config::DataCollection::Allow => "allow",
                    },
                ),
                // Resolved, not raw: an unset `wire` still has an effective shape
                // (the endpoint-exact heuristic), so render what the backend will
                // actually do, not just what was configured.
                wire: (*kind == crate::credentials::ProviderKind::Openai).then_some(
                    if b.uses_responses_wire() {
                        "responses"
                    } else {
                        "chat"
                    },
                ),
            };
            (name.clone(), doc)
        })
        .collect();

    let casts: BTreeMap<String, CastDoc> = config
        .casts
        .iter()
        .map(|(name, cast)| {
            let slots: CastDoc = cast
                .slots
                .iter()
                .map(|(role, slot)| {
                    // Exhaustive destructure, same discipline as Backend above.
                    let ModelSlot {
                        backend: _,
                        id: _,
                        vision: _,
                        max_tokens,
                        thinking_budget,
                        temperature,
                        effort,
                        thinking_style,
                        preamble,
                        lane,
                    } = slot;
                    let caps = config
                        .slot_caps(slot)
                        .expect("a loaded cast's slot backend resolves");
                    // Resolve the slot's request shape so we can flag tunables it will
                    // never send (e.g. a budget on an effort-driven model) — making the
                    // invisible no-op visible rather than rendering it as if effective.
                    let backend = config
                        .resolve_backend(&slot.backend)
                        .expect("a loaded cast's slot backend resolves");
                    let inert_tunables = if role.is_reasoning() {
                        // Resolve through `slot.tunables` — the same fallbacks the wire
                        // path takes, so a `[defaults].thinking_style` (which the old
                        // `unwrap_or_default()` here silently ignored) can't make the
                        // render disagree with what actually ships.
                        let t = slot.tunables(*role, &config.defaults);
                        let shape = ModelShape::resolve(backend.kind, &slot.id, t.thinking_style);
                        // Lane-aware: a `lane = "batch"` slot never builds an interactive
                        // arm. Batch POSTs its own body and gates the Responses shape on
                        // the endpoint-exact `is_hosted_openai`; the interactive arm
                        // follows the configurable `uses_responses_wire` (a responses-wire
                        // gateway shapes like Platform even though it isn't
                        // batch-eligible).
                        let batch_lane = matches!(lane, Some(Lane::Batch));
                        let responses_wire = if batch_lane {
                            backend.is_hosted_openai()
                        } else {
                            backend.uses_responses_wire()
                        };
                        let mut inert = Vec::new();
                        // Whether the effort ships is a policy with exactly one
                        // implementation — `Config::effort_disposition`. The render asks
                        // it rather than re-deriving (drop? batch lift? off-switch?),
                        // which is what keeps this and the startup warning from ever
                        // telling an operator different stories about the same slot.
                        let disposition = config
                            .effort_disposition(slot, *role)
                            .expect("a loaded cast's slot backend resolves");
                        // Batch hands the provider no sampling at all (`None`/`None`), so
                        // a `temperature` on a batch slot is inert regardless of the model.
                        let sampling_sinks = if batch_lane {
                            false
                        } else if responses_wire {
                            crate::consult::hosted_openai_accepts_sampling(&slot.id)
                        } else {
                            shape.sinks_sampling()
                        };
                        if thinking_budget.is_some() && !shape.sinks_thinking_budget() {
                            inert.push("thinking_budget");
                        }
                        // The *effective* effort, not just a per-slot override: a
                        // `[defaults]`/env effort lands on every cast, and one that lands
                        // nowhere deserves the same flag. Inherited built-in defaults stay
                        // quiet — see `Defaults::explorer_effort_explicit`.
                        if disposition.explicit && !disposition.ships_as_configured() {
                            inert.push("effort");
                        }
                        if temperature.is_some() && !sampling_sinks {
                            inert.push("temperature");
                        }
                        // The Anthropic thinking-style escape hatch only reaches the
                        // wire on an Anthropic slot — `ModelShape::resolve` reads `ovr`
                        // in exactly one match arm. Every other wire classifies its
                        // thinking style from the model id alone and never consults the
                        // override, so a style forced on this slot (a per-slot override
                        // or an inherited `[defaults].thinking_style`, both already
                        // folded into `t.thinking_style`) is silently dropped there.
                        // `Auto` stays quiet: it is the no-override default and behaves
                        // identically to an absent override on every wire, Anthropic
                        // included.
                        // `Off` is honored on EVERY wire (it short-circuits before the
                        // per-wire match), so it is never inert — only the Anthropic
                        // TIERS are, and only away from Anthropic.
                        if matches!(
                            t.thinking_style,
                            ThinkingStyleOverride::Adaptive | ThinkingStyleOverride::Budget
                        ) && backend.kind.wire() != Some(WireKind::Anthropic)
                        {
                            inert.push("thinking_style");
                        }
                        inert
                    } else {
                        // A media slot sends one generation request with no reasoning
                        // phase, so every reasoning knob WRITTEN on the slot is inert —
                        // and the request-shaping machinery (`ModelShape`,
                        // `effort_disposition`) is deliberately not resolved for it (see
                        // `ModelRole::is_reasoning`). Same single policy the startup
                        // warning reads (`Config::media_tunable_diagnostics`); inherited
                        // `[defaults]` values stay quiet.
                        slot.written_reasoning_tunables()
                    };
                    (
                        role.key(),
                        SlotDoc {
                            model: slot.qualified(),
                            vision: caps.vision,
                            max_tokens: *max_tokens,
                            thinking_budget: *thinking_budget,
                            temperature: *temperature,
                            effort: effort.clone(),
                            thinking_style: thinking_style.map(|s| format!("{s:?}").to_lowercase()),
                            preamble: preamble.clone(),
                            lane: lane.map(Lane::as_str),
                            inert_tunables,
                        },
                    )
                })
                .collect();
            (name.clone(), slots)
        })
        .collect();

    // Exhaustive destructures, same discipline as Backend/ModelSlot above: a new
    // field on any of these is a compile error here, forcing an explicit
    // render-or-skip decision instead of silently vanishing from the resource.
    let &ToolGating {
        consult,
        explore,
        deliberate,
        oneshot,
        run_kaish,
        batch,
        list_models,
        generate,
    } = &config.tools;
    let crate::sandbox::SandboxConfig {
        exec_timeout,
        output_limit_bytes,
        scratch_limit_bytes,
        disable_builtins,
        ignore,
    } = &config.sandbox;
    let crate::config::Defaults {
        explorer_max_turns,
        synth_max_turns,
        max_tokens,
        thinking_budget,
        explorer_temperature,
        synth_temperature,
        top_p,
        explorer_effort,
        synth_effort,
        // Provenance, not a knob: it decides whether an inert effort is worth flagging
        // (below and at startup), and the render already shows the effective values.
        explorer_effort_explicit: _,
        synth_effort_explicit: _,
        thinking_style,
        request_timeout,
        call_deadline,
        session_capacity,
        job_capacity,
        inline_attach_budget,
        max_attachments,
    } = &config.defaults;
    let crate::config::TelemetryConfig {
        enabled: telemetry_enabled,
        endpoint: telemetry_endpoint,
        logs: telemetry_logs,
        // Read through `resolve_logs_endpoint` below, which is where the derivation
        // lives; the raw override alone would under-report where records go.
        logs_endpoint: _,
        headers: telemetry_headers,
        timeout: telemetry_timeout,
        service_name: telemetry_service_name,
    } = &config.telemetry;
    let doc = ConfigDoc {
        allowed_paths: allowed_set
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        default_root: default_root.map(|p| p.display().to_string()),
        default_root_inferred,
        default_cast: config.default_cast.clone(),
        runtime: RuntimeDoc {
            follow_worktrees: config.follow_worktrees,
            followed_worktrees: followed_worktrees
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            advertised_tools: super::live_tools(config, &usable)
                .into_iter()
                .map(str::to_string)
                .collect(),
            unstaffable_tools: unstaffable,
        },
        tools: ToolsDoc {
            consult,
            explore,
            deliberate,
            oneshot,
            run_kaish,
            batch,
            list_models,
            generate,
        },
        sandbox: SandboxDoc {
            exec_timeout_secs: exec_timeout.as_secs(),
            output_limit_bytes: *output_limit_bytes,
            scratch_limit_bytes: *scratch_limit_bytes,
            disable_builtins: disable_builtins.clone(),
        },
        kaish: KaishDoc {
            ignore: IgnoreDoc {
                files: ignore.files().to_vec(),
                defaults: ignore.use_defaults(),
                auto_gitignore: ignore.auto_gitignore(),
                global_gitignore: ignore.use_global_gitignore(),
                scope: match ignore.scope() {
                    kaish_kernel::IgnoreScope::Enforced => "enforced",
                    kaish_kernel::IgnoreScope::Advisory => "advisory",
                },
            },
        },
        defaults: DefaultsDoc {
            explorer_max_turns: *explorer_max_turns,
            synth_max_turns: *synth_max_turns,
            max_tokens: *max_tokens,
            thinking_budget: *thinking_budget,
            explorer_temperature: *explorer_temperature,
            synth_temperature: *synth_temperature,
            top_p: *top_p,
            explorer_effort: explorer_effort.clone(),
            synth_effort: synth_effort.clone(),
            thinking_style: format!("{thinking_style:?}").to_lowercase(),
            request_timeout_secs: request_timeout.as_secs(),
            call_deadline_secs: call_deadline.as_secs(),
            session_capacity: session_capacity.get(),
            job_capacity: job_capacity.get(),
            inline_attach_budget: *inline_attach_budget,
            max_attachments: *max_attachments,
        },
        telemetry: TelemetryDoc {
            enabled: *telemetry_enabled,
            endpoint: telemetry_endpoint.clone(),
            logs: *telemetry_logs,
            // A non-derivable endpoint is a fatal startup error, so a *running*
            // server always resolves here. Reported absent rather than guessed if
            // that ever stops being true.
            logs_endpoint: (*telemetry_logs)
                .then(|| crate::telemetry::resolve_logs_endpoint(&config.telemetry).ok())
                .flatten(),
            timeout_secs: telemetry_timeout.as_secs(),
            service_name: telemetry_service_name.clone(),
            header_names: telemetry_headers.keys().cloned().collect(),
        },
        persistence: PersistenceDoc {
            enabled: config.persistence.enabled,
            path: config
                .persistence
                .path
                .as_ref()
                .map(|p| p.display().to_string()),
            active: persistence_active,
        },
        cas: CasDoc {
            enabled: config.cas.enabled,
            mode: cas_mode.as_str().to_string(),
            dir: config.cas.dir.as_ref().map(|p| p.display().to_string()),
            max_bytes: config.cas.max_bytes,
            backing: cas_ephemeral_fs.map(|fs| {
                format!(
                    "{fs} — EPHEMERAL: artifacts here will not survive this container \
                         or host. Mount a volume at [cas] dir to keep them."
                )
            }),
        },
        artifacts: ArtifactsDoc {
            enabled: config.artifacts.enabled,
        },
        backend_aliases: config.backend_aliases().clone(),
        backends,
        cast_aliases: config.cast_aliases().clone(),
        casts,
    };

    // Serialize to TOML. If the TOML serializer rejects something (unlikely given
    // all fields are primitive strings/ints/bools), crash loudly rather than return
    // a silently truncated or misleading document — the caller would get a half-truth.
    let body = toml::to_string_pretty(&doc).expect(
        "config render structs are TOML-serializable; if this panics, a field type changed",
    );
    // Prepend a comment block that explains how to widen the allowed set — the tool
    // descriptions promise kaibo://config tells a caller how to do this.
    format!(
        "# kaibo resolved runtime configuration\n\
         # To widen the allowed path set:\n\
         #   CLI:    --allow-path DIR  (repeatable)\n\
         #   env:    KAIBO_ALLOW_PATHS=DIR:DIR2  (colon-separated)\n\
         #   config: [server] allow_paths = [\"DIR\"] in config.toml\n\
         # A non-empty --allow-path list replaces the env/file layer.\n\n\
         {body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[runtime]` section surfaces the live follow state: the knob, plus the
    /// worktrees admitted *beyond* the static allowed set right now (passed in by
    /// the handler, which computes them at read time). This keeps `kaibo://config`
    /// honest about the real boundary — an auto-followed sibling isn't in
    /// `allowed_paths` but is reachable, and a reader must be able to see that.
    #[test]
    fn config_resource_runtime_section_reports_followed_worktrees() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/the-repo")];
        let followed = vec![std::path::PathBuf::from("/tmp/the-repo-feature")];
        let body = render_config_resource(
            &config,
            &allowed,
            None,
            false,
            followed,
            false,
            crate::config::CasMode::Memory,
            None,
        );
        assert!(
            body.contains("[runtime]") && body.contains("follow_worktrees = true"),
            "runtime section must echo the follow knob:\n{body}"
        );
        assert!(
            body.contains("/tmp/the-repo-feature"),
            "runtime section must list the followed worktree:\n{body}"
        );
    }

    /// A backend's `kind` renders as its canonical (hyphenated) name, the spelling an
    /// operator writes in config — a Debug-derived lowercase would collapse
    /// `openai-images` into `openaiimages`, a name that round-trips into a load error.
    #[test]
    fn config_resource_renders_kind_by_canonical_name() {
        let config = Config::from_toml_str(
            r#"
            [backends.imggen]
            kind = "openai-images"
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        assert!(
            body.contains(r#"kind = "openai-images""#),
            "the kind must render in its canonical spelling:\n{body}"
        );
        assert!(
            !body.contains("openaiimages"),
            "a Debug-lowercased kind name must not appear:\n{body}"
        );
    }

    /// The body of TOML section `header` — up to the next `[` at column 0. A
    /// `BTreeMap` field renders as its own nested table (`[runtime.unstaffable_tools]`)
    /// rather than inline, so a test that wants one has to ask for it by name.
    fn section<'a>(body: &'a str, header: &str) -> &'a str {
        body.split(&format!("\n{header}\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("no `{header}` section in:\n{body}"))
            .split("\n[")
            .next()
            .expect("the section's own body")
    }

    /// `[runtime]` answers "why can't I see that tool?" for the case an operator cannot
    /// otherwise diagnose: the tool is gone because nothing can staff it, not because
    /// they turned it off. On the built-in config that's `deliberate` — no built-in cast
    /// pairs an explorer with an offline synth — so it must be absent from
    /// `advertised_tools` AND named in `[runtime.unstaffable_tools]` with the cast shape
    /// that would fix it. Without both halves the tool just vanishes, which is the
    /// confusing failure this reporting exists to prevent.
    #[test]
    fn config_resource_runtime_explains_a_tool_no_cast_can_staff() {
        let body = render_config_resource(
            &Config::builtin(),
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let runtime = section(&body, "[runtime]");
        assert!(
            !runtime.contains("\"deliberate\""),
            "deliberate can't be staffed by any built-in cast, so it must not appear in \
             advertised_tools:\n{runtime}"
        );
        // A targeted drop, not a shutdown — the staffable surface is still advertised.
        assert!(
            runtime.contains("\"consult\"") && runtime.contains("\"run_kaish\""),
            "advertised_tools must list the surviving surface:\n{runtime}"
        );

        let unstaffable = section(&body, "[runtime.unstaffable_tools]");
        assert!(
            unstaffable.contains("deliberate = "),
            "unstaffable_tools must name deliberate:\n{unstaffable}"
        );
        assert!(
            unstaffable.to_lowercase().contains("offline synth"),
            "the entry must say what shape of cast would bring the tool back, not just \
             that it is missing:\n{unstaffable}"
        );
    }

    /// The other half: a tool the OPERATOR disabled is not reported as unstaffable.
    /// Conflating "you switched it off" with "nothing can run it" would send an operator
    /// hunting for a cast to fix a flag they set themselves.
    #[test]
    fn config_resource_does_not_blame_casts_for_an_operator_disabled_tool() {
        let mut config = Config::builtin();
        config.tools.deliberate = false;
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        assert!(
            !section(&body, "[runtime.unstaffable_tools]").contains("deliberate"),
            "a flag-disabled tool belongs to [tools], not unstaffable_tools:\n{body}"
        );
        assert!(
            section(&body, "[tools]").contains("deliberate = false"),
            "[tools] must still report the operator's own choice:\n{body}"
        );
    }

    /// A per-slot tunable that the slot's resolved request shape will never send is
    /// flagged `inert_tunables` in the render, so the operator sees the no-op instead of
    /// a knob that looks effective. The matrix: a budget on an effort-driven model
    /// (Gemini 3-line, Anthropic adaptive) or the toggle-less openai path; an effort on
    /// a budget model; a temperature an Anthropic slot drops under thinking. A knob that
    /// *does* have a sink is never flagged.
    #[test]
    fn config_render_flags_inert_per_slot_tunables() {
        let config = Config::from_toml_str(
            r#"
            # Gemini 3-line: takes thinkingLevel (effort), no budget.
            [casts.gem]
            explorer = { backend = "gemini", id = "gemini-3-pro", thinking_budget = 4096, effort = "low" }

            # openai (toggle-less): sends neither effort nor budget; keeps sampling.
            [casts.oai]
            synth = { backend = "openai-local", id = "gemma-local", thinking_budget = 8192, effort = "high", temperature = 0.7 }

            # Anthropic budget tier: takes budget_tokens, no effort; drops sampling under thinking.
            [casts.ant_budget]
            explorer = { backend = "anthropic", id = "claude-haiku-4-5", effort = "high", temperature = 0.5 }

            # Anthropic adaptive: takes output_config.effort, no budget.
            [casts.ant_adaptive]
            synth = { backend = "anthropic", id = "claude-opus-4-8", effort = "high", thinking_budget = 2048 }

            [backends.gpt]
            kind = "openai"
            base_url = "https://api.openai.com/v1"

            # Hosted GPT-5 reasoning family: sinks effort, not budget or sampling.
            [casts.gpt_reasoning]
            synth = { backend = "gpt", id = "gpt-5.6-sol", effort = "high", thinking_budget = 2048, temperature = 0.7 }

            # Older hosted GPT chat family: sinks sampling, not reasoning effort or budget.
            [casts.gpt_chat]
            synth = { backend = "gpt", id = "gpt-4.1-mini", effort = "high", thinking_budget = 2048, temperature = 0.7 }
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let inert = |cast: &str, role: &str| -> Vec<String> {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .map(|a| {
                    a.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            inert("gem", "explorer"),
            vec!["thinking_budget"],
            "Gemini 3-line sinks effort (thinkingLevel) but not a budget"
        );
        assert_eq!(
            inert("oai", "synth"),
            vec!["thinking_budget"],
            "an openai-compatible wire carries `reasoning_effort`, so effort SHIPS; only \
             a token budget has no sink there, and temperature it does send"
        );
        assert_eq!(
            inert("ant_budget", "explorer"),
            vec!["effort", "temperature"],
            "budget tier ignores effort; Anthropic drops sampling under thinking"
        );
        assert_eq!(
            inert("ant_adaptive", "synth"),
            vec!["thinking_budget"],
            "adaptive sinks effort but rejects a budget"
        );
        assert_eq!(
            inert("gpt_reasoning", "synth"),
            vec!["thinking_budget", "temperature"],
            "hosted GPT reasoning sinks effort but rejects budget and sampling"
        );
        assert_eq!(
            inert("gpt_chat", "synth"),
            vec!["thinking_budget", "effort"],
            "hosted GPT chat sinks sampling but rejects reasoning effort and budget"
        );
    }

    /// An image slot never runs a reasoning phase, so the render judges it by what the
    /// operator WROTE on the slot, not by any wire shape: every written reasoning knob
    /// is flagged inert, and a bare image slot stays clean even when `[defaults]`
    /// carries a written synth effort (inherited values are a fallback artifact, not a
    /// statement about the image slot). Same rule as the startup warning
    /// (`Config::media_tunable_diagnostics`) — one policy, two surfaces.
    #[test]
    fn config_render_flags_written_reasoning_knobs_on_an_image_slot() {
        let config = Config::from_toml_str(
            r#"
            [defaults]
            # Written, synth-side: lands on reasoning slots, must NOT flag image slots.
            synth_effort = "medium"

            [backends.sd]
            kind = "stability"

            # Knobs written on the image slot itself: all inert, all flagged.
            [casts.noisy]
            synth = "deepseek/deepseek-v4-pro"
            image = { backend = "sd", id = "core", thinking_budget = 4096, effort = "high", temperature = 0.5, thinking_style = "adaptive" }

            # A bare image slot: nothing written, nothing flagged.
            [casts.clean]
            synth = "deepseek/deepseek-v4-pro"
            image = "sd/ultra"
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let inert = |cast: &str, role: &str| -> Vec<String> {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .map(|a| {
                    a.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            inert("noisy", "image"),
            vec!["thinking_budget", "effort", "temperature", "thinking_style"],
            "every reasoning knob written on the image slot is flagged"
        );
        assert!(
            inert("clean", "image").is_empty(),
            "a bare image slot is clean; the written [defaults] synth effort does not \
             leak onto it"
        );
    }

    /// The three blind spots that let an inert knob render as effective, closed together
    /// because they share one cause — the render used to resolve a slot's shape by its
    /// own rules instead of asking the same question the wire asks.
    ///
    /// 1. **`[defaults]`-sourced effort.** The check keyed on a per-*slot* `effort`, so a
    ///    `[defaults].synth_effort` — which lands on every cast at once — rendered as
    ///    effective on a wire that drops it. The effective value is what matters.
    /// 2. **`[defaults].thinking_style`.** The render resolved the style with
    ///    `unwrap_or_default()` (→ `Auto`) while the wire uses the `[defaults]` fallback,
    ///    so a forced `adaptive` moved the sinks on the wire and not in the render.
    /// 3. **Lane-blindness.** A `lane = "batch"` slot never builds an interactive arm:
    ///    batch hands the provider no sampling at all, and floors the effort — so a rung
    ///    at or below `BATCH_EFFORT` is lifted and the configured value never ships.
    #[test]
    fn config_render_resolves_inert_tunables_the_way_the_wire_does() {
        let config = Config::from_toml_str(
            r#"
            [defaults]
            # Lands on every synth slot below; effective, not per-slot.
            synth_effort = "medium"
            # The wire honors this fallback; the render used to ignore it.
            thinking_style = "adaptive"

            [backends.lab]
            kind = "openai"
            base_url = "http://127.0.0.1:8080/v1"
            key_optional = true

            # An openai-compatible wire: the defaults effort DOES reach it, but the
            # [defaults] thinking_style tier does not — only Anthropic reads a tier.
            [casts.lab_cast]
            synth = "lab/gemma"

            # Forced adaptive gives Haiku an effort sink and takes away its budget one —
            # the opposite of what the classifier alone would say.
            [casts.forced]
            synth = { backend = "anthropic", id = "claude-haiku-4-5", thinking_budget = 2048 }

            # A batch slot: sampling is never sent, and `low` is below the batch floor.
            [casts.bat]
            synth = { backend = "anthropic", id = "claude-opus-4-8", lane = "batch", effort = "low", temperature = 0.4 }

            # Same lane, a rung deeper than the floor: it ships as written now that batch
            # floors rather than overrides, so it is NOT inert.
            [casts.bat_deep]
            synth = { backend = "anthropic", id = "claude-opus-4-8", lane = "batch", effort = "xhigh" }
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let inert = |cast: &str, role: &str| -> Vec<String> {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .map(|a| {
                    a.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            inert("lab_cast", "synth"),
            vec!["thinking_style"],
            "a [defaults] effort now REACHES an openai-compatible wire, so it is no \
             longer inert; the [defaults] thinking_style tier forced onto a \
             non-Anthropic wire still is, because only Anthropic reads a tier"
        );
        assert_eq!(
            inert("forced", "synth"),
            vec!["thinking_budget"],
            "[defaults].thinking_style = adaptive moves both sinks — the render must \
             resolve it the way slot.tunables does"
        );
        assert_eq!(
            inert("bat", "synth"),
            vec!["effort", "temperature"],
            "batch lifts a below-floor effort and sends no sampling at all"
        );
        assert!(
            inert("bat_deep", "synth").is_empty(),
            "a batch slot deeper than the floor keeps its effort: {:?}",
            inert("bat_deep", "synth")
        );
    }

    /// `thinking_style` reaches the wire only on an Anthropic slot —
    /// `ModelShape::resolve` reads the override in exactly one match arm (Anthropic's,
    /// picking adaptive vs budget tier). Every other wire classifies its thinking
    /// style from the model id alone and never looks at the override, so a style
    /// forced on a non-Anthropic slot is a pure no-op there — the render must flag it
    /// the same way it flags an inert `thinking_budget`/`effort`/`temperature`, not
    /// show it as if it were shaping the request.
    #[test]
    fn config_render_flags_thinking_style_inert_on_a_non_anthropic_slot() {
        let config = Config::from_toml_str(
            r#"
            # Forced on a DeepSeek slot: DeepSeek's shape never consults the override.
            [casts.ds_forced]
            synth = { backend = "deepseek", id = "deepseek-v4-pro", thinking_style = "adaptive" }

            # The identical override on an Anthropic slot moves the wire's shape (it
            # picks the adaptive tier over Haiku's default budget tier), so it must
            # NOT be flagged.
            [casts.anthropic_forced]
            synth = { backend = "anthropic", id = "claude-haiku-4-5", thinking_style = "adaptive" }
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let inert = |cast: &str, role: &str| -> Vec<String> {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .map(|a| {
                    a.as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            inert("ds_forced", "synth"),
            vec!["thinking_style"],
            "a thinking_style override on a DeepSeek slot is never consulted by \
             ModelShape::resolve and must render as inert"
        );
        assert!(
            inert("anthropic_forced", "synth").is_empty(),
            "the same override on an Anthropic slot moves the wire's shape and must \
             stay unflagged: {:?}",
            inert("anthropic_forced", "synth")
        );
    }

    /// The startup warning and the `kaibo://config` render must give the same verdict on
    /// the same slot. They are two audiences for one policy, and when they were two
    /// implementations they diverged inside a single commit: the render already knew a
    /// batch-lane lift meant the configured value never runs, while the startup scan only
    /// asked "does this wire have a reasoning field?" and stayed silent — so an operator
    /// who set `effort = "low"` on a batch slot saw it flagged in the resource and never
    /// heard a word at startup.
    ///
    /// Asserting *agreement* rather than either verdict is deliberate: it fails for any
    /// future divergence, not just this one. `Config::effort_disposition` is the single
    /// implementation both now read.
    #[test]
    fn the_startup_scan_and_the_config_render_agree_slot_for_slot() {
        let config = Config::from_toml_str(
            r#"
            [backends.lab]
            kind = "openai"
            base_url = "http://127.0.0.1:8080/v1"
            key_optional = true

            # The case that diverged: an effort-carrying wire, but the batch depth floor
            # lifts this shallower value, so `low` never runs.
            [casts.bat_low]
            synth = { backend = "anthropic", id = "claude-opus-4-8", lane = "batch", effort = "low" }

            # Deeper than the floor: ships as written, nobody should complain.
            [casts.bat_deep]
            synth = { backend = "anthropic", id = "claude-opus-4-8", lane = "batch", effort = "xhigh" }

            # Reasoning off on the batch lane: a depth floor must not raise it, so it
            # ships as configured and is NOT a diagnostic.
            [casts.bat_off]
            synth = { backend = "anthropic", id = "claude-opus-4-8", lane = "batch", effort = "none" }

            # Reasoning switched off — the drop. An openai-compatible wire carries
            # `reasoning_effort` on its own, so `thinking_style = "off"` is what makes
            # this slot's effort go nowhere.
            [casts.lab_cast]
            synth = { backend = "lab", id = "gemma", effort = "xhigh", thinking_style = "off" }

            # Carries it fine.
            [casts.ds]
            synth = { backend = "deepseek", id = "deepseek-v4-pro", effort = "xhigh" }
            "#,
        )
        .unwrap();

        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let render_flags = |cast: &str, role: &str| -> bool {
            doc.get("casts")
                .and_then(|c| c.get(cast))
                .and_then(|c| c.get(role))
                .and_then(|s| s.get("inert_tunables"))
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|v| v.as_str() == Some("effort")))
                .unwrap_or(false)
        };
        let warned: std::collections::BTreeSet<(String, &str)> = config
            .effort_diagnostics()
            .into_iter()
            .map(|d| (d.cast, d.role))
            .collect();

        for cast in ["bat_low", "bat_deep", "bat_off", "lab_cast", "ds"] {
            assert_eq!(
                warned.contains(&(cast.to_string(), "synth")),
                render_flags(cast, "synth"),
                "cast {cast}: the startup scan and the kaibo://config render disagree \
                 about whether this slot's effort ships"
            );
        }

        // And the verdicts themselves, so "they agree" can't be satisfied by both being
        // wrong in the same direction.
        assert!(
            render_flags("bat_low", "synth"),
            "a lifted effort is flagged"
        );
        assert!(
            render_flags("lab_cast", "synth"),
            "a dropped effort is flagged"
        );
        assert!(!render_flags("bat_deep", "synth"), "a deeper effort ships");
        assert!(
            !render_flags("bat_off", "synth"),
            "the off-switch survives a depth floor, so it ships as configured"
        );
        assert!(!render_flags("ds", "synth"), "deepseek carries the effort");
    }

    /// `kaibo://config` renders each openai-kind backend's *resolved* wire — the
    /// answer `uses_responses_wire` gives, not the raw configured value — so an
    /// unset `wire` still shows an effective shape. Absent on every other kind.
    #[test]
    fn config_render_shows_resolved_wire_for_openai_backends() {
        let config = Config::from_toml_str(
            r#"
            [backends.gpt]
            kind = "openai"
            base_url = "https://api.openai.com/v1"

            [backends.gateway]
            kind = "openai"
            base_url = "https://llm-gateway.example.internal/v1"
            wire = "responses"

            [backends.onprem]
            kind = "openai"
            base_url = "http://localhost:13399/api/v1"
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let doc: toml::Value = toml::from_str(&body).expect("render is valid TOML");
        let wire = |name: &str| -> Option<String> {
            doc.get("backends")
                .and_then(|b| b.get(name))
                .and_then(|b| b.get("wire"))
                .and_then(|w| w.as_str())
                .map(str::to_string)
        };
        assert_eq!(
            wire("gpt").as_deref(),
            Some("responses"),
            "unset wire on OpenAI Platform's own endpoint resolves to responses"
        );
        assert_eq!(
            wire("gateway").as_deref(),
            Some("responses"),
            "an explicit wire = \"responses\" renders as configured"
        );
        assert_eq!(
            wire("onprem").as_deref(),
            Some("chat"),
            "unset wire on a local server resolves to chat"
        );
        assert_eq!(
            wire("anthropic"),
            None,
            "wire is absent on every non-openai kind"
        );
    }

    /// The config resource body must contain the key structural fields a calling
    /// model or operator expects: allowed paths, default_cast, gated tools,
    /// sandbox limits, backends with kind and key sources, and casts with their
    /// slots rendered as "backend/id" carrying resolved caps.
    #[test]
    fn config_resource_renders_expected_fields() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/test-allowed")];
        let body = render_config_resource(
            &config,
            &allowed,
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        // Structural presence checks — the resource is TOML or a document, not prose.
        for needle in [
            "allowed_paths",
            "default_cast",
            "[runtime]",
            "follow_worktrees",
            "tools",
            "sandbox",
            "defaults",
            "backends",
            "casts",
        ] {
            assert!(
                body.contains(needle),
                "config resource must contain {needle:?}:\n{body}"
            );
        }
        // The allowed path we passed must appear.
        assert!(
            body.contains("/tmp/test-allowed"),
            "config resource must show the allowed set:\n{body}"
        );
        // Backends and casts include the built-in five.
        for name in [
            "anthropic",
            "deepseek",
            "gemini",
            "openrouter",
            "openai-local",
        ] {
            assert!(
                body.contains(&format!("[backends.{name}]")),
                "config resource must list the {name} backend:\n{body}"
            );
            assert!(
                body.contains(&format!("casts.{name}")),
                "config resource must list the {name} cast:\n{body}"
            );
        }
        // Slots render as "backend/id" with their RESOLVED caps (the classifier on
        // the slot's backend kind: Anthropic sees, DeepSeek is blind).
        assert!(
            body.contains("anthropic/claude-sonnet-4-6"),
            "slots render as backend/id:\n{body}"
        );
        let anthropic_synth = body
            .find("anthropic/claude-sonnet-4-6")
            .map(|i| &body[i..i + 120])
            .unwrap();
        assert!(
            anthropic_synth.contains("vision = true"),
            "anthropic slot carries resolved vision=true:\n{anthropic_synth}"
        );
        let deepseek_synth = body
            .find("deepseek/deepseek-v4-pro")
            .map(|i| &body[i..i + 120])
            .unwrap();
        assert!(
            deepseek_synth.contains("vision = false"),
            "deepseek slot carries resolved vision=false:\n{deepseek_synth}"
        );
        // Key SOURCES (env var name / file path) must appear — operators configured
        // them and need to see them for diagnostics.
        assert!(
            body.contains("ANTHROPIC_API_KEY"),
            "config resource must show key source env var names:\n{body}"
        );
        // Telemetry state is part of the resolved runtime: an operator must be able
        // to see whether kaibo is shipping spans off-box and to where.
        assert!(
            body.contains("[telemetry]") && body.contains("enabled = false"),
            "config resource must show telemetry state (off by default):\n{body}"
        );
    }

    /// SECRET-SAFETY teeth: an export header *value* (e.g. a bearer token) must
    /// never reach the rendered resource — only the header *name*, the pointer the
    /// operator set, exactly as key sources render their env var name not the key.
    #[test]
    fn config_resource_withholds_telemetry_header_values() {
        let config = Config::from_toml_str(
            r#"
            [telemetry]
            enabled = true
            headers = { authorization = "Bearer super-secret-token" }
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        // The header NAME is discoverable…
        assert!(
            body.contains("authorization"),
            "header name must be visible for diagnostics:\n{body}"
        );
        // …but its VALUE is a secret and must not leak.
        assert!(
            !body.contains("super-secret-token") && !body.contains("Bearer"),
            "a header value must never render — it can be a bearer token:\n{body}"
        );
    }

    /// Persistence state is part of the resolved runtime: an operator must see whether
    /// the durable store is on, where its db lives, and whether it actually opened.
    #[test]
    fn config_resource_shows_persistence_state() {
        // Enabled (the default) with the store open.
        let config =
            Config::from_toml_str("[persistence]\npath = \"/var/lib/kaibo/state.db\"\n").unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            true,
            crate::config::CasMode::Disk,
            None,
        );
        let section = body
            .split_once("[persistence]")
            .expect("a [persistence] table renders")
            .1;
        assert!(
            section.contains("enabled = true")
                && section.contains("/var/lib/kaibo/state.db")
                && section.contains("active = true"),
            "enabled store must show on, its resolved path, and active:\n{body}"
        );

        // Disabled: off and inactive.
        let off = Config::from_toml_str("[persistence]\nenabled = false\n").unwrap();
        let body = render_config_resource(
            &off,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        let section = body.split_once("[persistence]").expect("table renders").1;
        assert!(
            section.contains("enabled = false") && section.contains("active = false"),
            "a disabled store renders off and inactive:\n{body}"
        );
    }

    /// The media CAS renders its knob AND its runtime mode — the three-way state
    /// (disk / memory / off) an operator needs to see, with `"memory"` the loud
    /// degraded case (artifacts do not survive a restart) and `dir` shown even then
    /// so a diagnosing operator sees where disk mode would write.
    #[test]
    fn config_resource_shows_cas_state_in_all_three_modes() {
        use crate::config::CasMode;

        let config = Config::from_toml_str("[cas]\ndir = \"/srv/art\"\n").unwrap();
        let body =
            render_config_resource(&config, &[], None, false, vec![], true, CasMode::Disk, None);
        let section = body.split_once("[cas]").expect("a [cas] table renders").1;
        assert!(
            section.contains("enabled = true")
                && section.contains(r#"mode = "disk""#)
                && section.contains("/srv/art"),
            "disk mode must show on, the mode word, and the dir:\n{body}"
        );

        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            CasMode::Memory,
            None,
        );
        let section = body.split_once("[cas]").expect("table renders").1;
        assert!(
            section.contains(r#"mode = "memory""#) && section.contains("/srv/art"),
            "memory mode must render as memory and still show the configured dir:\n{body}"
        );

        let off = Config::from_toml_str("[cas]\nenabled = false\n").unwrap();
        let body =
            render_config_resource(&off, &[], None, false, vec![], false, CasMode::Off, None);
        let section = body.split_once("[cas]").expect("table renders").1;
        assert!(
            section.contains("enabled = false") && section.contains(r#"mode = "off""#),
            "the explicit off switch renders as off:\n{body}"
        );
    }

    /// The alias registries are part of the resolved runtime state: an alias is a
    /// valid `cast` value and slot-ref prefix, so a caller reading `kaibo://config`
    /// must be able to discover them — built-ins and file-declared both.
    #[test]
    fn config_resource_renders_backend_and_cast_aliases() {
        let config = Config::from_toml_str(
            r#"
            [backends.big]
            kind = "openai"
            base_url = "http://localhost:9001/v1"
            aliases = ["heavy"]

            [casts.team]
            aliases = ["fast"]
            synth = "heavy/qwen3-235b"
            "#,
        )
        .unwrap();
        let body = render_config_resource(
            &config,
            &[],
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        for needle in ["[backend_aliases]", "[cast_aliases]"] {
            assert!(body.contains(needle), "must render {needle}:\n{body}");
        }
        // Built-in aliases at both levels, and the file-declared ones.
        for needle in [
            r#"claude = "anthropic""#,
            r#"google = "gemini""#,
            r#"heavy = "big""#,
            r#"fast = "team""#,
        ] {
            assert!(body.contains(needle), "must render {needle}:\n{body}");
        }
    }

    /// SECRET-SAFETY: the config resource must expose key SOURCE metadata (env var
    /// names, file paths), but NEVER the resolved key values.  We set a sentinel in
    /// the environment and in a temp file, render the resource, and assert the
    /// sentinel appears nowhere in the output.
    ///
    /// `set_var`/`remove_var` are UB when other threads call `getenv` concurrently
    /// (glibc). A mutex serializes the env-touching half against any sibling unit
    /// test in this binary that touches env (there are none today, but the lock is
    /// cheap and structural). The file half needs no mutex.
    #[test]
    fn config_resource_never_exposes_key_values() {
        use std::io::Write;
        use std::sync::Mutex;
        const SENTINEL: &str = "SUPER_SECRET_KEY_VALUE_12345_CANARY";
        // Module-level lock — serializes all set_var/remove_var in this test binary.
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        let var_name = "KAIBO_TEST_SECRET_ENV_VAR_CANARY";
        let allowed = vec![std::path::PathBuf::from("/tmp")];

        // Build the config outside the lock (no env access yet).
        let toml = format!("[backends.anthropic]\napi_key_env = \"{var_name}\"\n");
        let config = Config::from_toml_str(&toml).expect("valid config");

        // Set the sentinel in env and render inside the lock.
        let body = {
            let _guard = ENV_LOCK.lock().unwrap();
            // SAFETY: holding the lock means no other test in this binary mutates env.
            #[allow(deprecated)]
            unsafe {
                std::env::set_var(var_name, SENTINEL);
            }
            let b = render_config_resource(
                &config,
                &allowed,
                None,
                false,
                vec![],
                false,
                crate::config::CasMode::Memory,
                None,
            );
            #[allow(deprecated)]
            unsafe {
                std::env::remove_var(var_name);
            }
            b
        };

        // The env var *name* must appear (operator needs to see what's configured).
        assert!(
            body.contains(var_name),
            "config resource must show the env var name (not value):\n{body}"
        );
        // The sentinel value must NOT appear — this is the invariant.
        assert!(
            !body.contains(SENTINEL),
            "config resource must NEVER expose the API key value; \
             sentinel found in:\n{body}"
        );

        // The file half needs no env access — no lock needed.
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "{SENTINEL}").expect("write sentinel");
        let file_path = tmp.path().to_string_lossy().to_string();
        let toml2 = format!("[backends.anthropic]\napi_key_file = \"{file_path}\"\n");
        let config2 = Config::from_toml_str(&toml2).expect("valid config");
        let body2 = render_config_resource(
            &config2,
            &allowed,
            None,
            false,
            vec![],
            false,
            crate::config::CasMode::Memory,
            None,
        );
        // The file path (source pointer) may appear, but not the file contents.
        assert!(
            !body2.contains(SENTINEL),
            "config resource must NEVER expose key file contents; \
             sentinel found in:\n{body2}"
        );
    }
}
