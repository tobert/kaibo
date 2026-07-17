//! The `kaibo://config` resource renderer — the resolved runtime configuration
//! serialized to an annotated TOML document (allowed trees, gated tools, sandbox
//! limits, backends and casts), with the render-only `*Doc` shapes it builds from.
//! Renders key *source* metadata (env var names, key-file paths), never resolved
//! secret values.

use std::path::{Path, PathBuf};

use crate::config::{Config, Lane, ModelSlot};
use crate::consult::ModelShape;

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
pub(super) fn render_config_resource(
    config: &Config,
    allowed_set: &[PathBuf],
    default_root: Option<&Path>,
    default_root_inferred: bool,
    followed_worktrees: Vec<PathBuf>,
    persistence_active: bool,
) -> String {
    use serde::Serialize;
    use std::collections::BTreeMap;

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
    }

    /// Runtime-computed scope state. `follow_worktrees` echoes the knob;
    /// `followed_worktrees` is the live extra set the follow feature grants beyond
    /// `allowed_paths` right now — git worktrees of an already-allowed repo,
    /// resolved by reading git's link files. Recomputed on each read, so a worktree
    /// added mid-session shows up here without a reconnect.
    #[derive(Serialize)]
    struct RuntimeDoc {
        follow_worktrees: bool,
        followed_worktrees: Vec<String>,
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
    }

    /// Telemetry as resolved. SECRET-SAFETY: `header_names` lists the keys of any
    /// configured export headers but never their values — an Authorization value is
    /// a secret, same as an API key. The operator set the names; surfacing those is
    /// the discoverability the resource promises.
    #[derive(Serialize)]
    struct TelemetryDoc {
        enabled: bool,
        endpoint: String,
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

    #[derive(Serialize)]
    struct BackendDoc {
        kind: String,
        /// Resolved endpoint for openai-kind backends (explicit base_url, else
        /// OPENAI_BASE_URL, else the built-in default) — the "resolved runtime
        /// state" promise. Other kinds have fixed endpoints baked into rig.
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
        /// Per-slot tunables that *are* set here but this slot's resolved request shape
        /// will never send — the honest no-op flag. A `thinking_budget` on an
        /// effort-driven or toggle-less model, an `effort` on a budget model, a
        /// `temperature` an Anthropic slot drops under thinking: each load-validates and
        /// would otherwise render as if effective. Absent when every set knob has a sink.
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
            } = b;
            let rendered_base_url = if *kind == crate::credentials::ProviderKind::Openai {
                Some(b.resolved_base_url())
            } else {
                base_url.clone()
            };
            let doc = BackendDoc {
                kind: format!("{:?}", kind).to_lowercase(),
                base_url: rendered_base_url,
                // KEY SOURCE ONLY — env var name or file path, never the value.
                api_key_env: api_key_env.clone(),
                api_key_file: api_key_file.clone(),
                key_optional: *key_optional,
                request_timeout_secs: request_timeout.as_secs(),
                data_collection: (*kind == crate::credentials::ProviderKind::OpenRouter)
                    .then_some(match data_collection {
                        crate::config::DataCollection::Deny => "deny",
                        crate::config::DataCollection::Allow => "allow",
                    }),
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
                    let kind = config
                        .resolve_backend(&slot.backend)
                        .expect("a loaded cast's slot backend resolves")
                        .kind;
                    let shape =
                        ModelShape::resolve(kind, &slot.id, thinking_style.unwrap_or_default());
                    let mut inert_tunables = Vec::new();
                    if thinking_budget.is_some() && !shape.sinks_thinking_budget() {
                        inert_tunables.push("thinking_budget");
                    }
                    if effort.is_some() && !shape.sinks_effort() {
                        inert_tunables.push("effort");
                    }
                    if temperature.is_some() && !shape.sinks_sampling() {
                        inert_tunables.push("temperature");
                    }
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
        thinking_style,
        request_timeout,
        call_deadline,
        session_capacity,
        job_capacity,
        inline_attach_budget,
    } = &config.defaults;
    let crate::config::TelemetryConfig {
        enabled: telemetry_enabled,
        endpoint: telemetry_endpoint,
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
        },
        tools: ToolsDoc {
            consult,
            explore,
            deliberate,
            oneshot,
            run_kaish,
            batch,
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
        },
        telemetry: TelemetryDoc {
            enabled: *telemetry_enabled,
            endpoint: telemetry_endpoint.clone(),
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
        let body = render_config_resource(&config, &allowed, None, false, followed, false);
        assert!(
            body.contains("[runtime]") && body.contains("follow_worktrees = true"),
            "runtime section must echo the follow knob:\n{body}"
        );
        assert!(
            body.contains("/tmp/the-repo-feature"),
            "runtime section must list the followed worktree:\n{body}"
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
            "#,
        )
        .unwrap();
        let body = render_config_resource(&config, &[], None, false, vec![], false);
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
            vec!["thinking_budget", "effort"],
            "openai sends neither thinking knob; temperature it does send"
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
    }

    /// The config resource body must contain the key structural fields a calling
    /// model or operator expects: allowed paths, default_cast, gated tools,
    /// sandbox limits, backends with kind and key sources, and casts with their
    /// slots rendered as "backend/id" carrying resolved caps.
    #[test]
    fn config_resource_renders_expected_fields() {
        let config = Config::builtin();
        let allowed = vec![std::path::PathBuf::from("/tmp/test-allowed")];
        let body = render_config_resource(&config, &allowed, None, false, vec![], false);
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
        // Backends and casts include the built-in four.
        for name in ["anthropic", "deepseek", "gemini", "openai-local"] {
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
        let body = render_config_resource(&config, &[], None, false, vec![], false);
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
        let body = render_config_resource(&config, &[], None, false, vec![], true);
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
        let body = render_config_resource(&off, &[], None, false, vec![], false);
        let section = body.split_once("[persistence]").expect("table renders").1;
        assert!(
            section.contains("enabled = false") && section.contains("active = false"),
            "a disabled store renders off and inactive:\n{body}"
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
        let body = render_config_resource(&config, &[], None, false, vec![], false);
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
            let b = render_config_resource(&config, &allowed, None, false, vec![], false);
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
        let body2 = render_config_resource(&config2, &allowed, None, false, vec![], false);
        // The file path (source pointer) may appear, but not the file contents.
        assert!(
            !body2.contains(SENTINEL),
            "config resource must NEVER expose key file contents; \
             sentinel found in:\n{body2}"
        );
    }
}
