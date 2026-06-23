//! Config loading for the backends/casts split: built-in equivalence, chimera
//! casts, alias resolution at both levels, and the loud-failure invariants.
//! Pure where possible — `from_toml_str` touches no env or filesystem; the
//! env/CLI layers are exercised through injectable seams. The contract is
//! `docs/casts.md`.

use std::collections::HashMap;
use std::time::Duration;

use kaibo::config::{
    default_models, parse_slot_ref, Backend, Config, ModelRole, ModelSlot, ToolDisables,
};
use kaibo::consult::ThinkingStyleOverride;
use kaibo::credentials::{openai_base_url, ProviderKind, PLACEHOLDER_OPENAI_KEY};
use kaibo::server::ToolGating;

// --- Built-in equivalence ---------------------------------------------------
// A missing config file and `cast = "anthropic"` reproduce kaibo's historical
// behavior byte-for-byte (docs/casts.md "Built-in equivalence").

#[test]
fn builtin_reproduces_the_historical_defaults() {
    let c = Config::builtin();

    // Turn caps are set high on purpose (a capable model rarely wastes turns and a
    // cap-hit now degrades gracefully rather than failing) — see Defaults::default.
    assert_eq!(c.defaults.explorer_max_turns, 100);
    assert_eq!(c.defaults.synth_max_turns, 200);
    // Token/thinking budgets still match the old ConsultConfig + THINKING_BUDGET.
    assert_eq!(c.defaults.max_tokens, 16384);
    assert_eq!(c.defaults.thinking_budget, 8192);
    // The per-request LLM deadline default: 15 min (see Defaults::default).
    assert_eq!(c.defaults.request_timeout, Duration::from_secs(900));

    // Four built-in backends + four same-named single-backend casts, carrying
    // the model ids that used to live on the profiles.
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::Openai,
    ] {
        let name = kind.canonical_name();
        let backend = c.resolve_backend(name).unwrap();
        assert_eq!(backend.kind, kind);
        assert_eq!(backend.request_timeout, Duration::from_secs(900));

        let cast = c.resolve_cast(name).unwrap();
        let (explorer, synth) = default_models(kind);
        let e = cast.require_slot(ModelRole::Explorer).unwrap();
        let s = cast.require_slot(ModelRole::Synth).unwrap();
        assert_eq!(e.id, explorer, "{kind:?} explorer");
        assert_eq!(s.id, synth, "{kind:?} synth");
        // Built-in casts are single-backend, named after their kind.
        assert_eq!(e.backend, name);
        assert_eq!(s.backend, name);

        // Tunables inherit the defaults: a bare built-in slot resolves to the
        // historical values for both roles.
        let et = e.tunables(ModelRole::Explorer, &c.defaults);
        let st = s.tunables(ModelRole::Synth, &c.defaults);
        assert_eq!(et.max_tokens, 16384);
        assert_eq!(et.thinking_budget, 8192);
        assert_eq!(et.temperature, 0.1, "{kind:?} cold explorer");
        assert_eq!(st.temperature, 0.3, "{kind:?} warmer synth");
        assert_eq!(et.top_p, 0.95);
        assert_eq!(et.effort, "high");
        assert_eq!(st.effort, "high");
        assert_eq!(et.thinking_style, ThinkingStyleOverride::Auto);
    }

    // Default cast is anthropic; no root unless configured; all tools on.
    assert_eq!(c.default_cast, "anthropic");
    assert!(c.root.is_none());
    assert_eq!(c.tools, ToolGating::default());
}

#[test]
fn a_missing_default_path_yields_builtins_not_an_error() {
    // No file at the default location is fine — kaibo runs out of the box, and
    // the result is byte-for-byte the built-in registry.
    let c = Config::load_with(None, Some("/nonexistent/kaibo/config.toml".into()), |_| {
        None
    })
    .expect("absent default config must not error");
    let builtin = Config::builtin();
    assert_eq!(c.default_cast, builtin.default_cast);
    assert_eq!(c.defaults, builtin.defaults);
    assert_eq!(c.backends, builtin.backends);
    assert_eq!(c.casts, builtin.casts);
}

#[test]
fn a_missing_explicit_path_is_an_error() {
    // An explicit --config / KAIBO_CONFIG that doesn't exist is a mistake, loud.
    let err =
        Config::load_with(Some("/nonexistent/kaibo.toml".into()), None, |_| None).unwrap_err();
    assert!(format!("{err:#}").contains("not found"), "got: {err:#}");
}

// --- Alias resolution at both levels -----------------------------------------
// The built-in profile aliases became BOTH cast aliases (so `cast = "claude"`
// resolves) and backend aliases (so a slot ref `claude/<id>` resolves).

#[test]
fn builtin_aliases_resolve_at_both_levels() {
    let c = Config::builtin();
    // Cast level.
    assert_eq!(c.resolve_cast("claude").unwrap().name, "anthropic");
    assert_eq!(c.resolve_cast("google").unwrap().name, "gemini");
    for a in ["local", "lemonade", "gemma", "gemma4"] {
        assert_eq!(
            c.resolve_cast(a).unwrap().name,
            "openai",
            "{a:?} should alias the openai cast"
        );
    }
    // Backend level.
    assert_eq!(c.resolve_backend("claude").unwrap().name, "anthropic");
    assert_eq!(c.resolve_backend("google").unwrap().name, "gemini");
    for a in ["local", "lemonade", "gemma", "gemma4"] {
        assert_eq!(
            c.resolve_backend(a).unwrap().name,
            "openai",
            "{a:?} should alias the openai backend"
        );
    }
}

#[test]
fn a_slot_ref_written_against_an_alias_canonicalizes() {
    // "claude/<id>" in a slot resolves through the backend alias map and is
    // stored canonical, so caps classify and `kaibo://config` renders the same
    // slot regardless of which spelling the file used.
    let c = Config::from_toml_str(
        r#"
        [casts.x]
        synth = "claude/claude-opus-4-8"
        "#,
    )
    .unwrap();
    let slot = c
        .resolve_cast("x")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap();
    assert_eq!(slot.backend, "anthropic");
    assert_eq!(slot.qualified(), "anthropic/claude-opus-4-8");
    // And the slot classifies on the (anthropic) backend kind.
    assert!(c.slot_caps(slot).unwrap().vision);
}

#[test]
fn file_declared_aliases_resolve_at_both_levels() {
    let c = Config::from_toml_str(
        r#"
        [backends.big]
        kind = "openai"
        base_url = "http://localhost:9001/v1"
        aliases = ["heavy"]

        [casts.team]
        aliases = ["fast", "smart"]
        synth = "heavy/qwen3-235b"
        "#,
    )
    .unwrap();
    // The backend alias resolves directly AND inside a slot ref.
    assert_eq!(c.resolve_backend("heavy").unwrap().name, "big");
    let slot = c
        .resolve_cast("team")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap();
    assert_eq!(slot.backend, "big", "slot ref through a file alias");
    // The cast aliases resolve.
    assert_eq!(c.resolve_cast("fast").unwrap().name, "team");
    assert_eq!(c.resolve_cast("smart").unwrap().name, "team");
}

// --- The headline: a chimera cast --------------------------------------------

#[test]
fn a_chimera_cast_spans_backends_with_both_slot_forms() {
    // The use case the split exists for (docs/casts.md "Why"): deepseek explorer,
    // claude synth, local image gen — one composed thing selected by one name.
    let c = Config::from_toml_str(
        r#"
        [backends.sd]
        kind = "openai"
        base_url = "http://localhost:7860/v1"
        key_optional = true

        [casts.chimera]
        explorer = "deepseek/deepseek-v4-flash"
        synth = { backend = "claude", id = "claude-opus-4-8", effort = "max", max_tokens = 32768 }
        image = "sd/sdxl-turbo"
        tts = "gemini/gemini-2.5-flash-tts"
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("chimera").unwrap();
    let e = cast.require_slot(ModelRole::Explorer).unwrap();
    let s = cast.require_slot(ModelRole::Synth).unwrap();
    let i = cast.require_slot(ModelRole::Image).unwrap();
    let t = cast.require_slot(ModelRole::Tts).unwrap();

    // String form parses as backend/id; table form carries its tunables.
    assert_eq!(e.qualified(), "deepseek/deepseek-v4-flash");
    assert_eq!(s.qualified(), "anthropic/claude-opus-4-8");
    assert_eq!(s.effort.as_deref(), Some("max"));
    assert_eq!(s.max_tokens, Some(32768));
    assert_eq!(i.qualified(), "sd/sdxl-turbo");
    assert_eq!(t.qualified(), "gemini/gemini-2.5-flash-tts");

    // Four slots, four different backends — the fused profile could never say this.
    let backends: std::collections::BTreeSet<&str> = [e, s, i, t]
        .iter()
        .map(|slot| slot.backend.as_str())
        .collect();
    assert_eq!(backends.len(), 4, "every role on its own backend");
}

#[test]
fn slot_refs_split_on_the_first_slash_only() {
    // HuggingFace-style ids keep their inner slash: only the FIRST `/` splits.
    let (backend, id) = parse_slot_ref("openai/Qwen/Qwen3-32B").unwrap();
    assert_eq!(backend, "openai");
    assert_eq!(id, "Qwen/Qwen3-32B");
    // And the same through the TOML string form.
    let c = Config::from_toml_str(
        r#"
        [casts.hf]
        synth = "openai/Qwen/Qwen3-32B"
        "#,
    )
    .unwrap();
    let slot = c
        .resolve_cast("hf")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap();
    assert_eq!(slot.id, "Qwen/Qwen3-32B");

    let err = parse_slot_ref("no-slash-here").unwrap_err();
    assert!(
        format!("{err:#}").contains("must be \"backend/model-id\""),
        "got: {err:#}"
    );
    assert!(parse_slot_ref("/id-only").is_err());
    assert!(parse_slot_ref("backend/").is_err());
}

#[test]
fn a_file_cast_stanza_merges_role_wise_over_a_builtin() {
    // Retarget just the anthropic synth; the explorer keeps its built-in id.
    let c = Config::from_toml_str(
        r#"
        [casts.anthropic]
        synth = "anthropic/claude-opus-4-8"
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("anthropic").unwrap();
    assert_eq!(
        cast.require_slot(ModelRole::Synth).unwrap().id,
        "claude-opus-4-8"
    );
    assert_eq!(
        cast.require_slot(ModelRole::Explorer).unwrap().id,
        default_models(ProviderKind::Anthropic).0
    );
}

#[test]
fn media_roles_are_absent_until_configured() {
    // Absent = capability absent, not an error (docs/casts.md): built-in casts
    // carry only the agent roles, and require_slot names the gap loudly.
    let c = Config::builtin();
    let cast = c.resolve_cast("anthropic").unwrap();
    assert!(cast.slot(ModelRole::Image).is_none());
    assert!(cast.slot(ModelRole::Tts).is_none());
    let err = cast.require_slot(ModelRole::Tts).unwrap_err();
    assert!(
        format!("{err:#}").contains("has no tts slot"),
        "got: {err:#}"
    );
}

// --- Caps classify on the SLOT's backend kind ---------------------------------

#[test]
fn caps_classify_on_the_slots_backend_kind() {
    // A chimera's slots straddle a capability line: the deepseek explorer is
    // blind, the anthropic synth sees — each classified on ITS backend's kind.
    let c = Config::from_toml_str(
        r#"
        [casts.chimera]
        explorer = "deepseek/deepseek-v4-flash"
        synth = "claude/claude-sonnet-4-6"
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("chimera").unwrap();
    let e = cast.require_slot(ModelRole::Explorer).unwrap();
    let s = cast.require_slot(ModelRole::Synth).unwrap();
    assert!(!c.slot_caps(e).unwrap().vision, "deepseek is text-only");
    assert!(c.slot_caps(s).unwrap().vision, "anthropic is multimodal-in");
}

#[test]
fn a_vision_pin_on_a_slot_wins_over_the_classifier() {
    // The escape hatch pins in BOTH directions: a vision model behind a generic
    // openai endpoint opts in; a pin can also force a seeing kind blind.
    let c = Config::from_toml_str(
        r#"
        [casts.x]
        explorer = { backend = "openai", id = "llava-13b", vision = true }
        synth = { backend = "anthropic", id = "claude-sonnet-4-6", vision = false }
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("x").unwrap();
    let e = cast.require_slot(ModelRole::Explorer).unwrap();
    let s = cast.require_slot(ModelRole::Synth).unwrap();
    assert!(
        c.slot_caps(e).unwrap().vision,
        "openai-kind classifies blind; the pin opts in"
    );
    assert!(
        !c.slot_caps(s).unwrap().vision,
        "anthropic classifies seeing; the pin opts out"
    );
}

// --- Two openai endpoints, both live (the regression that motivated profiles) --

#[test]
fn two_openai_backends_resolve_to_distinct_endpoints() {
    let c = Config::from_toml_str(
        r#"
        [backends.gpt]
        kind = "openai"
        base_url = "https://api.openai.com/v1"

        [backends.llama]
        kind = "openai"
        base_url = "http://localhost:8080/v1"

        [casts.hosted]
        synth = "gpt/gpt-5"

        [casts.kitchen]
        synth = "llama/qwen2.5-coder-32b"
        "#,
    )
    .unwrap();
    let gpt = c.resolve_backend("gpt").unwrap();
    let llama = c.resolve_backend("llama").unwrap();
    assert_eq!(gpt.kind, ProviderKind::Openai);
    assert_eq!(llama.kind, ProviderKind::Openai);
    assert_eq!(gpt.resolved_base_url(), "https://api.openai.com/v1");
    assert_eq!(llama.resolved_base_url(), "http://localhost:8080/v1");
    // The built-ins are still present alongside.
    assert!(c.resolve_backend("anthropic").is_ok());
    assert!(c.resolve_cast("openai").is_ok());
}

#[test]
fn a_builtin_openai_backend_without_base_url_uses_the_env_default() {
    // No explicit base_url → the resolved URL is whatever OPENAI_BASE_URL/default
    // yields (env-robust check).
    let c = Config::builtin();
    let b = c.resolve_backend("openai").unwrap();
    assert_eq!(b.resolved_base_url(), openai_base_url());
}

// --- Per-slot tunables: override or inherit the per-role [defaults] -----------

#[test]
fn per_slot_tunables_override_defaults_others_inherit() {
    let c = Config::from_toml_str(
        r#"
        [defaults]
        max_tokens = 20000
        thinking_budget = 9000
        synth_temperature = 0.5

        [casts.tuned]
        explorer = { backend = "deepseek", id = "deepseek-v4-flash", temperature = 0.0 }
        synth = { backend = "anthropic", id = "claude-opus-4-8", max_tokens = 32768, thinking_budget = 16384 }
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("tuned").unwrap();

    // The synth slot overrides both budget knobs; temperature inherits the
    // file-set synth default.
    let st = cast
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(st.max_tokens, 32768);
    assert_eq!(st.thinking_budget, 16384);
    assert_eq!(st.temperature, 0.5);

    // The explorer slot overrides only temperature; budgets inherit [defaults].
    let et = cast
        .require_slot(ModelRole::Explorer)
        .unwrap()
        .tunables(ModelRole::Explorer, &c.defaults);
    assert_eq!(et.temperature, 0.0);
    assert_eq!(et.max_tokens, 20000);
    assert_eq!(et.thinking_budget, 9000);

    // A built-in cast that overrode nothing inherits the file-set defaults too.
    let a = c
        .resolve_cast("anthropic")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(a.max_tokens, 20000);
    assert_eq!(a.thinking_budget, 9000);
    assert_eq!(a.temperature, 0.5);
}

#[test]
fn effort_and_thinking_style_default_and_override_per_slot() {
    // Built-in defaults: "high" both roles, Auto classification.
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.defaults.explorer_effort, "high");
    assert_eq!(c.defaults.synth_effort, "high");
    assert_eq!(c.defaults.thinking_style, ThinkingStyleOverride::Auto);

    let c = Config::from_toml_str(
        r#"
        [defaults]
        synth_effort = "max"

        [casts.anthropic]
        explorer = { backend = "anthropic", id = "claude-haiku-4-5", effort = "low", thinking_style = "adaptive" }
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("anthropic").unwrap();
    // The explorer slot overrides effort and thinking_style.
    let et = cast
        .require_slot(ModelRole::Explorer)
        .unwrap()
        .tunables(ModelRole::Explorer, &c.defaults);
    assert_eq!(et.effort, "low");
    assert_eq!(et.thinking_style, ThinkingStyleOverride::Adaptive);
    // The untouched synth slot inherits the file's synth_effort and Auto style.
    let st = cast
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(st.effort, "max");
    assert_eq!(st.thinking_style, ThinkingStyleOverride::Auto);
}

// --- Loud failures (crash over silent degrade) --------------------------------

#[test]
fn malformed_toml_is_an_error() {
    assert!(Config::from_toml_str("this is not = = valid toml").is_err());
}

#[test]
fn a_profiles_table_is_a_tombstone_naming_the_contract() {
    // [profiles] is deleted, not deprecated: a leftover table — any shape — is a
    // load error pointing at docs/casts.md, never a silent reinterpretation.
    let err = Config::from_toml_str(
        r#"
        [profiles.anthropic]
        synth_model = "claude-opus-4-8"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("[profiles]"), "got: {msg}");
    assert!(msg.contains("docs/casts.md"), "got: {msg}");
    assert!(msg.contains("[backends"), "got: {msg}");
    assert!(msg.contains("[casts"), "got: {msg}");
}

#[test]
fn env_kaibo_provider_is_a_tombstone_naming_kaibo_cast() {
    // The old selector env var must not be silently ignored into the default cast.
    let err = Config::load_with(None, None, |k| {
        (k == "KAIBO_PROVIDER").then(|| "anthropic".to_string())
    })
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("KAIBO_PROVIDER"), "got: {msg}");
    assert!(msg.contains("KAIBO_CAST"), "got: {msg}");
}

#[test]
fn an_unknown_backend_in_a_slot_names_the_known_backends() {
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = "nope/some-model"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("unknown backend \"nope\""), "got: {msg}");
    assert!(msg.contains("known backends"), "got: {msg}");
    for known in ["anthropic", "deepseek", "gemini", "openai"] {
        assert!(msg.contains(known), "should name {known}, got: {msg}");
    }
}

#[test]
fn an_unknown_role_key_in_a_cast_is_rejected() {
    // The role keys are struct fields under deny_unknown_fields: a typo'd role
    // must fail loudly, not silently configure nothing.
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        explorr = "anthropic/claude-haiku-4-5"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("explorr"), "names the bad key, got: {msg}");
}

#[test]
fn a_typoed_slot_tunable_is_rejected_naming_the_key() {
    // The table form is deny_unknown_fields too — a misspelled knob must not
    // silently vanish, and the error must name the fix (the bad key and the
    // valid knobs), not hide it behind untagged-enum dispatch ("data did not
    // match any variant" names neither).
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = "claude-sonnet-4-6", max_tokenz = 9000 }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("max_tokenz"), "names the bad key, got: {msg}");
    assert!(
        msg.contains("max_tokens"),
        "names the valid knobs, got: {msg}"
    );
}

#[test]
fn an_empty_model_id_is_rejected_loudly() {
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = " " }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("model id is empty"), "got: {msg}");
    assert!(msg.contains("synth"), "names the role, got: {msg}");
}

#[test]
fn alias_collisions_are_loud_at_each_level() {
    // A user cast named like a built-in cast alias collides.
    let err = Config::from_toml_str(
        r#"
        [casts.claude]
        synth = "anthropic/claude-opus-4-8"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("cast alias \"claude\""), "got: {msg}");
    assert!(msg.contains("collides"), "got: {msg}");

    // A user backend named like a built-in backend alias collides. (base_url set
    // so the new-openai-backend rule doesn't fire first — the collision is the
    // thing under test.)
    let err = Config::from_toml_str(
        "[backends.google]\nkind = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n",
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("backend alias \"google\""), "got: {msg}");
    assert!(msg.contains("collides"), "got: {msg}");

    // A file alias colliding with a real built-in name is rejected.
    let err = Config::from_toml_str(
        r#"
        [casts.mine]
        aliases = ["anthropic"]
        synth = "anthropic/claude-sonnet-4-6"
        "#,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("collides"), "got: {err:#}");

    // Two casts claiming the same alias collide ("claimed by both").
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
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("claimed by both"),
        "got: {err:#}"
    );
}

#[test]
fn base_url_on_a_keyed_backend_is_rejected() {
    // rig fixes the keyed kinds' endpoints; a base_url there is a config mistake.
    let err = Config::from_toml_str(
        r#"
        [backends.anthropic]
        base_url = "https://example.test/v1"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("base_url"), "got: {msg}");
    assert!(msg.contains("only the `openai` kind"), "got: {msg}");
}

#[test]
fn a_new_backend_without_a_kind_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [backends.mystery]
        base_url = "http://localhost:1/v1"
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("must declare a `kind`"),
        "got: {err:#}"
    );
}

#[test]
fn redeclaring_a_backends_kind_differently_is_rejected() {
    let err = Config::from_toml_str("[backends.anthropic]\nkind = \"gemini\"\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("already exists as kind"),
        "got: {err:#}"
    );
}

#[test]
fn a_new_openai_backend_without_base_url_is_rejected_loudly() {
    // A user-declared openai-kind backend with a forgotten base_url would
    // silently dial the global default endpoint (OPENAI_BASE_URL or the local
    // llama.cpp server) — a wrong-server 404 mid-call. Only the built-in
    // `openai` backend keeps that fallback; a new stanza must say where it points.
    let err = Config::from_toml_str(
        r#"
        [backends.sd]
        kind = "openai"
        key_optional = true
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("base_url"),
        "names the missing key, got: {msg}"
    );
    assert!(msg.contains("sd"), "names the backend, got: {msg}");
    // The built-in `openai` backend keeps the env/default fallback: overriding
    // it without base_url stays valid, and a config-less load is unchanged.
    let c = Config::from_toml_str("[backends.openai]\nkey_optional = false\n").unwrap();
    assert!(c.resolve_backend("openai").unwrap().base_url.is_none());
}

#[test]
fn zero_request_timeout_is_rejected_loudly() {
    // A zero deadline times out every call instantly — a mistake, not a config.
    let err = Config::from_toml_str(
        r#"
        [backends.broken]
        kind = "openai"
        base_url = "http://localhost:1/v1"
        request_timeout_secs = 0
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("request_timeout_secs must be > 0"),
        "got: {err:#}"
    );
}

#[test]
fn an_inverted_thinking_budget_is_rejected_at_the_resolved_slot() {
    // Anthropic requires max_tokens > thinking_budget; catch the inverted pair at
    // load, not as a runtime 400 — validated on the slot's RESOLVED values.

    // Per-slot override pair, inverted.
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = "claude-sonnet-4-6", max_tokens = 1000, thinking_budget = 2000 }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("thinking_budget (2000)"), "got: {msg}");
    assert!(msg.contains("max_tokens (1000)"), "got: {msg}");

    // The inversion can also arrive purely through [defaults]: a global
    // max_tokens below the default 8192 budget breaks the built-in anthropic
    // and gemini slots at resolution time.
    let err = Config::from_toml_str("[defaults]\nmax_tokens = 4096\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("thinking_budget"),
        "got: {err:#}"
    );

    // The same inverted pair on a non-thinking-budget kind is accepted.
    Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "openai", id = "m", max_tokens = 1000, thinking_budget = 2000 }
        "#,
    )
    .expect("openai-kind slots have no budget/headroom coupling");
}

#[test]
fn an_unknown_default_cast_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [server]
        cast = "does-not-exist"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("does-not-exist"), "got: {msg}");
    assert!(msg.contains("server.cast"), "got: {msg}");
}

#[test]
fn the_old_server_provider_key_is_rejected() {
    // `[server] provider` was renamed to `cast`; deny_unknown_fields makes the
    // stale key a loud load error instead of a silently ignored selector.
    let err = Config::from_toml_str(
        r#"
        [server]
        provider = "anthropic"
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("provider"),
        "names the stale key, got: {err:#}"
    );
}

#[test]
fn out_of_range_sampling_is_a_loud_error() {
    // No silent clamp: a temperature past the accepted band is a typo, caught at load.
    let err = Config::from_toml_str("[defaults]\nsynth_temperature = 3.0\n").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("synth_temperature") && msg.contains("[0.0, 2.0]"),
        "got: {msg}"
    );

    // top_p must be a probability in (0, 1] — zero is rejected.
    let err = Config::from_toml_str("[defaults]\ntop_p = 0.0\n").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("top_p") && msg.contains("(0.0, 1.0]"),
        "got: {msg}"
    );

    // The per-slot temperature gets the same band check.
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "openai", id = "m", temperature = 3.0 }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("temperature") && msg.contains("[0.0, 2.0]"),
        "got: {msg}"
    );
}

#[test]
fn bad_thinking_style_is_a_loud_error() {
    // No silent fallback: a value outside auto|adaptive|budget is a typo.
    let err = Config::from_toml_str("[defaults]\nthinking_style = \"bogus\"\n").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("thinking_style") && msg.contains("bogus"),
        "got: {msg}"
    );
    // Per-slot too.
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = "m", thinking_style = "bogus" }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("thinking_style") && msg.contains("bogus"),
        "got: {msg}"
    );
}

#[test]
fn an_unknown_key_is_rejected() {
    // deny_unknown_fields: a typo'd knob must fail loudly, not silently no-op.
    let err = Config::from_toml_str(
        r#"
        [server]
        cazt = "openai"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("cazt") || msg.contains("unknown"),
        "got: {msg}"
    );
}

// --- File / env / CLI layering -------------------------------------------------

#[test]
fn env_kaibo_cast_overrides_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[server]\ncast = \"openai\"\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_CAST", "gemini")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.default_cast, "gemini");
}

#[test]
fn cli_cast_wins_over_env_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[server]\ncast = \"openai\"\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_CAST", "gemini")].into_iter().collect();
    let mut c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    c.apply_cli(
        Some("/tmp/proj".into()),
        Some("deepseek".to_string()),
        // Only --no-oneshot was passed.
        ToolDisables {
            oneshot: true,
            ..Default::default()
        },
        vec![], // no --allow-path flags
        false,  // --no-follow-worktrees not passed
        vec![], // no --project-context-file flags
        vec![], // no --user-context-file flags
    );
    assert_eq!(c.default_cast, "deepseek", "--cast beats env and file");
    assert_eq!(c.root.as_deref(), Some(std::path::Path::new("/tmp/proj")));
    // Only oneshot is dropped; the rest stay enabled.
    assert!(c.tools.consult);
    assert!(!c.tools.oneshot);
    assert!(c.tools.run_kaish);
}

/// An empty CLI `--allow-path` list (no flags passed) must NOT replace the lower
/// layers (env/file allow_paths). The guard at `apply_cli` is `if !allow_paths.is_empty()`
/// — this test pins it so an accidental unconditional assignment would kill the env/file
/// knobs without any test catching it.
#[test]
fn empty_cli_allow_paths_preserves_lower_layers() {
    let mut c = Config::builtin();
    // Pre-seed allow_paths as if they came from env or a config file.
    c.allow_paths = vec![std::path::PathBuf::from("/tmp/from-env")];
    // Apply CLI with no --allow-path flags (empty list).
    c.apply_cli(
        None,
        None,
        ToolDisables::default(),
        vec![],
        false,
        vec![],
        vec![],
    );
    // The env/file-layer value must survive.
    assert!(
        c.allow_paths
            .iter()
            .any(|p| p == std::path::Path::new("/tmp/from-env")),
        "empty CLI allow_paths must not replace env/file layers, got {:?}",
        c.allow_paths
    );
}

/// A leading `~` in `[server] root` and `allow_paths` must expand to `$HOME` — the
/// same tilde handling key files and `[context]` paths already get. A config file is
/// hand-edited, so `~/src` is the natural thing to write; taking it literally would
/// later canonicalize a bogus `~` path and refuse startup. Non-tilde paths pass
/// through untouched. (The env layer funnels through the same conversion, so this
/// covers `KAIBO_ROOT` / `KAIBO_ALLOW_PATHS` too.)
#[test]
fn tilde_expands_in_root_and_allow_paths() {
    // Trim a trailing slash so `{home}/src` matches `PathBuf::from(HOME).join("src")`,
    // which normalizes `/home/user/` + `src` to `/home/user/src` (no empty component).
    let home = std::env::var("HOME").expect("HOME set in test env");
    let home = home.trim_end_matches('/');
    let toml = "[server]\n\
                root = \"~/src/proj\"\n\
                allow_paths = [\"~/src\", \"/data/fixtures\"]\n";
    let c = Config::from_toml_str(toml).expect("valid config");

    assert_eq!(
        c.root.as_deref(),
        Some(std::path::Path::new(&format!("{home}/src/proj"))),
        "~ in [server] root must expand to $HOME"
    );
    assert!(
        c.allow_paths
            .contains(&std::path::PathBuf::from(format!("{home}/src"))),
        "~ in allow_paths must expand to $HOME, got {:?}",
        c.allow_paths
    );
    // A non-tilde absolute path is left exactly as written.
    assert!(
        c.allow_paths
            .contains(&std::path::PathBuf::from("/data/fixtures")),
        "absolute allow_paths must pass through untouched, got {:?}",
        c.allow_paths
    );
    // A literal `~` is never left dangling in either field.
    assert!(
        !c.allow_paths
            .iter()
            .any(|p| p.to_string_lossy().starts_with('~')),
        "no allow_paths entry may keep a literal leading ~, got {:?}",
        c.allow_paths
    );
}

/// The env layer funnels through the same expansion: `KAIBO_ROOT` and the
/// colon-separated `KAIBO_ALLOW_PATHS` must expand a leading `~` to `$HOME`. Pins the
/// commit's "covers env too" claim — distinct from the file-layer test — so a future
/// refactor that expanded only the file path would be caught.
#[test]
fn tilde_expands_in_env_layer_root_and_allow_paths() {
    let home = std::env::var("HOME").expect("HOME set in test env");
    let home = home.trim_end_matches('/');
    let env: HashMap<&str, &str> = [
        ("KAIBO_ROOT", "~/envroot"),
        ("KAIBO_ALLOW_PATHS", "~/a:~/b:/data/fixtures"),
    ]
    .into_iter()
    .collect();
    // No config file (built-in defaults) + the injected env layer.
    let c = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap();

    assert_eq!(
        c.root.as_deref(),
        Some(std::path::Path::new(&format!("{home}/envroot"))),
        "~ in KAIBO_ROOT must expand to $HOME"
    );
    for expected in [format!("{home}/a"), format!("{home}/b")] {
        assert!(
            c.allow_paths.contains(&std::path::PathBuf::from(&expected)),
            "~ in KAIBO_ALLOW_PATHS must expand to {expected}, got {:?}",
            c.allow_paths
        );
    }
    assert!(
        c.allow_paths
            .contains(&std::path::PathBuf::from("/data/fixtures")),
        "non-tilde KAIBO_ALLOW_PATHS entry must pass through, got {:?}",
        c.allow_paths
    );
}

#[test]
fn env_overrides_file_defaults_and_flows_into_slot_tunables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[defaults]\nmax_tokens = 11111\n").unwrap();

    // Both values sit above the 8192 thinking budget, so the built-in anthropic
    // slots stay valid; the point under test is env-over-file precedence.
    let env: HashMap<&str, &str> = [("KAIBO_MAX_TOKENS", "22222")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.max_tokens, 22222);
    // And the env'd default flows into a slot that inherits it.
    let t = c
        .resolve_cast("anthropic")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(t.max_tokens, 22222);
}

#[test]
fn a_non_numeric_env_tunable_is_a_loud_error() {
    let env: HashMap<&str, &str> = [("KAIBO_MAX_TOKENS", "lots")].into_iter().collect();
    let err = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap_err();
    assert!(
        format!("{err:#}").contains("KAIBO_MAX_TOKENS"),
        "got: {err:#}"
    );
}

#[test]
fn an_env_integer_tunable_above_i64_max_is_a_loud_error() {
    // TOML integers are i64, so the config-*file* path structurally can't carry
    // a larger value — but env can, and a quintillion-token budget is never an
    // intent. It would also panic the first `kaibo://config` read (the render
    // serializes the resolved value back to TOML). Loud at load instead.
    for (var, value) in [
        ("KAIBO_MAX_TOKENS", "9223372036854775808"), // i64::MAX + 1
        ("KAIBO_THINKING_BUDGET", "18446744073709551615"), // u64::MAX
        ("KAIBO_REQUEST_TIMEOUT_SECS", "9223372036854775808"),
        ("KAIBO_EXEC_TIMEOUT_SECS", "9223372036854775808"),
        ("KAIBO_OUTPUT_LIMIT_BYTES", "9223372036854775808"),
    ] {
        let env: HashMap<&str, &str> = [(var, value)].into_iter().collect();
        let err = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string()))
            .expect_err(&format!("{var}={value} must be rejected at load"));
        let msg = format!("{err:#}");
        assert!(msg.contains(var), "names the variable, got: {msg}");
    }
    // The boundary itself stays valid (i64::MAX is representable in TOML).
    let env: HashMap<&str, &str> = [("KAIBO_MAX_TOKENS", "9223372036854775807")]
        .into_iter()
        .collect();
    let c = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.max_tokens, i64::MAX as u64);
}

// --- request_timeout: defaults seed backends; per-backend override -------------

#[test]
fn request_timeout_seeds_from_defaults_and_overrides_per_backend() {
    // A slow local model wants a longer leash than a hosted API; the seam is a
    // [defaults] seed that a backend may raise (or lower) on its own.
    let c = Config::from_toml_str(
        r#"
        [defaults]
        request_timeout_secs = 120

        [backends.slowlocal]
        kind = "openai"
        base_url = "http://localhost:13305/api/v1"
        request_timeout_secs = 1800
        "#,
    )
    .unwrap();
    // The file-set default reseeds every built-in backend...
    assert_eq!(c.defaults.request_timeout, Duration::from_secs(120));
    assert_eq!(
        c.resolve_backend("anthropic").unwrap().request_timeout,
        Duration::from_secs(120)
    );
    // ...while the backend that overrode it keeps its own deadline.
    assert_eq!(
        c.resolve_backend("slowlocal").unwrap().request_timeout,
        Duration::from_secs(1800)
    );
}

#[test]
fn request_timeout_env_overrides_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[defaults]\nrequest_timeout_secs = 120\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_REQUEST_TIMEOUT_SECS", "45")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.request_timeout, Duration::from_secs(45));
    // And it reseeds backends that didn't override.
    assert_eq!(
        c.resolve_backend("anthropic").unwrap().request_timeout,
        Duration::from_secs(45)
    );
}

// --- Session capacity -----------------------------------------------------------

#[test]
fn session_capacity_defaults_and_overrides_from_file() {
    use std::num::NonZeroUsize;
    // Absent → the built-in 128.
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.defaults.session_capacity, NonZeroUsize::new(128).unwrap());
    // Set in [defaults] → honored.
    let c = Config::from_toml_str("[defaults]\nsession_capacity = 7\n").unwrap();
    assert_eq!(c.defaults.session_capacity, NonZeroUsize::new(7).unwrap());
}

#[test]
fn zero_session_capacity_is_rejected_loudly() {
    // A zero-capacity session cache can't be built and would mean "remember nothing"
    // — which omitting session_id already does. Crash at load, not on first session.
    let err = Config::from_toml_str("[defaults]\nsession_capacity = 0\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("session_capacity"),
        "got: {err:#}"
    );
}

// --- Key resolution (now a Backend concern) --------------------------------------

fn local_backend(api_key_file: Option<String>, key_optional: bool) -> Backend {
    Backend {
        name: "local".into(),
        kind: ProviderKind::Openai,
        base_url: Some("http://localhost:1/v1".into()),
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file,
        key_optional,
        request_timeout: Duration::from_secs(900),
    }
}

#[test]
fn key_optional_backend_falls_back_to_placeholder() {
    // A keyless backend whose env var is unset resolves to the placeholder, not an
    // error — the local-server case.
    let b = local_backend(None, true);
    assert_eq!(b.resolve_key().unwrap(), PLACEHOLDER_OPENAI_KEY);
}

#[test]
fn key_optional_backend_with_a_present_but_empty_key_file_errors() {
    // The no-silent-fallback invariant: a key file that's THERE but empty is a
    // mistake, not "keyless" — it must error, not quietly use the placeholder.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blank-key");
    std::fs::write(&file, "   \n").unwrap();
    let b = local_backend(Some(file.to_string_lossy().into_owned()), true);
    let err = b.resolve_key().unwrap_err();
    assert!(
        format!("{err:#}").contains("empty"),
        "a present-but-empty key file must error even when key_optional, got: {err:#}"
    );
}

#[test]
fn required_key_with_no_source_is_an_error() {
    let b = Backend {
        name: "needs-key".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: None,
        key_optional: false,
        request_timeout: Duration::from_secs(900),
    };
    let err = b.resolve_key().unwrap_err();
    assert!(
        format!("{err:#}").contains("needs-key"),
        "the error names the backend, got: {err:#}"
    );
}

// --- [sandbox] -------------------------------------------------------------------

#[test]
fn sandbox_defaults_when_unconfigured() {
    let c = Config::builtin();
    assert_eq!(c.sandbox.exec_timeout, Duration::from_secs(30));
    assert_eq!(c.sandbox.output_limit_bytes, 1 << 16); // 64 KiB default
    assert!(c.sandbox.disable_builtins.is_empty());
}

#[test]
fn sandbox_section_parses() {
    let c = Config::from_toml_str(
        r#"
        [sandbox]
        exec_timeout_secs = 5
        output_limit_bytes = 4096
        disable_builtins = ["grep", "find"]
        "#,
    )
    .unwrap();
    assert_eq!(c.sandbox.exec_timeout, Duration::from_secs(5));
    assert_eq!(c.sandbox.output_limit_bytes, 4096);
    assert_eq!(
        c.sandbox.disable_builtins,
        vec!["grep".to_string(), "find".to_string()]
    );
}

#[test]
fn sandbox_env_overrides_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[sandbox]\nexec_timeout_secs = 30\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_EXEC_TIMEOUT_SECS", "7")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.sandbox.exec_timeout, Duration::from_secs(7));
}

#[test]
fn validate_against_builtins_rejects_an_unknown_name() {
    let c = Config::from_toml_str(
        r#"
        [sandbox]
        disable_builtins = ["grep", "definitely-not-a-builtin"]
        "#,
    )
    .unwrap();
    let known = vec!["grep".to_string(), "cat".to_string(), "find".to_string()];
    let err = c.validate_against_builtins(&known).unwrap_err();
    assert!(
        format!("{err:#}").contains("definitely-not-a-builtin"),
        "an unknown disabled builtin must error loudly, got: {err:#}"
    );
}

#[test]
fn validate_against_builtins_accepts_a_known_subset() {
    let c = Config::from_toml_str(
        r#"
        [sandbox]
        disable_builtins = ["grep"]
        "#,
    )
    .unwrap();
    let known = vec!["grep".to_string(), "cat".to_string()];
    assert!(c.validate_against_builtins(&known).is_ok());
}

// --- The shipped example config ---------------------------------------------------

#[test]
fn the_shipped_example_config_parses() {
    // docs/config.example.toml documents the full surface; if it drifts from the
    // parser it teaches users a config that crashes at load. Keep it honest.
    let toml = include_str!("../docs/config.example.toml");
    let c = Config::from_toml_str(toml).expect("docs/config.example.toml must load");
    // Spot-check the headline example (docs/casts.md): the chimera cast spanning
    // backends, with at least the agent roles plus a media role.
    let chimera = c
        .resolve_cast("chimera")
        .expect("the example defines [casts.chimera]");
    let e = chimera.require_slot(ModelRole::Explorer).unwrap();
    let s = chimera.require_slot(ModelRole::Synth).unwrap();
    assert_eq!(e.backend, "deepseek", "explorer sweeps on deepseek");
    assert_eq!(
        s.backend, "anthropic",
        "synth answers on anthropic (claude/… refs canonicalize)"
    );
    assert!(
        chimera.slot(ModelRole::Image).is_some(),
        "the example carries a media role"
    );
    assert_ne!(
        e.backend, s.backend,
        "the example demonstrates a cross-backend cast"
    );
}

// --- ModelSlot conveniences (the pieces server.rs overrides lean on) ---------------

#[test]
fn a_bare_slot_carries_no_pins_or_tunables() {
    // `ModelSlot::bare` is the shape a per-call model override produces: the new
    // id classifies fresh, so no pin or tunable from the old slot may ride along.
    let slot = ModelSlot::bare("openai", "some-model");
    assert_eq!(slot.qualified(), "openai/some-model");
    assert_eq!(slot.vision, None);
    assert_eq!(slot.max_tokens, None);
    assert_eq!(slot.thinking_budget, None);
    assert_eq!(slot.temperature, None);
    assert_eq!(slot.effort, None);
    assert_eq!(slot.thinking_style, None);
}

// --- Telemetry (OTLP traces) -------------------------------------------------
// kaibo reads private source, so a default run must stay fully local: telemetry
// is opt-in, off by default. These are the teeth on that invariant plus the
// file/env precedence for the new [telemetry] table (mirrors [server]).

#[test]
fn telemetry_is_off_by_default_so_a_default_run_stays_local() {
    // The boundary that matters: a fresh install never ships a span off-box. If
    // someone flips the default to `true`, this fails — by design.
    let c = Config::builtin();
    assert!(
        !c.telemetry.enabled,
        "telemetry must default OFF — a default run ships nothing to a collector"
    );
    // The endpoint default points at a local OTLP/HTTP collector, so flipping
    // `enabled = true` alone targets localhost, not some remote.
    assert_eq!(c.telemetry.endpoint, "http://localhost:4318/v1/traces");
    assert_eq!(c.telemetry.service_name, "kaibo");
    assert!(c.telemetry.headers.is_empty());
}

#[test]
fn telemetry_table_parses_from_file() {
    let c = Config::from_toml_str(
        r#"
        [telemetry]
        enabled = true
        endpoint = "http://collector.internal:4318/v1/traces"
        timeout_secs = 5
        service_name = "kaibo-dev"
        headers = { authorization = "Bearer t0ken", "x-tenant" = "kaibo" }
        "#,
    )
    .unwrap();
    assert!(c.telemetry.enabled);
    assert_eq!(
        c.telemetry.endpoint,
        "http://collector.internal:4318/v1/traces"
    );
    assert_eq!(c.telemetry.timeout, Duration::from_secs(5));
    assert_eq!(c.telemetry.service_name, "kaibo-dev");
    assert_eq!(
        c.telemetry.headers.get("authorization").map(String::as_str),
        Some("Bearer t0ken")
    );
    assert_eq!(
        c.telemetry.headers.get("x-tenant").map(String::as_str),
        Some("kaibo")
    );
}

#[test]
fn env_overrides_telemetry_over_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // File turns it on and points somewhere; env retargets and retunes it.
    std::fs::write(
        &path,
        "[telemetry]\nenabled = true\nendpoint = \"http://file:4318/v1/traces\"\n",
    )
    .unwrap();
    let env: HashMap<&str, &str> = [
        ("KAIBO_TELEMETRY_ENDPOINT", "http://env:4318/v1/traces"),
        ("KAIBO_TELEMETRY_TIMEOUT_SECS", "30"),
        ("KAIBO_TELEMETRY_SERVICE_NAME", "kaibo-env"),
    ]
    .into_iter()
    .collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert!(
        c.telemetry.enabled,
        "file's enabled survives where env is silent"
    );
    assert_eq!(c.telemetry.endpoint, "http://env:4318/v1/traces");
    assert_eq!(c.telemetry.timeout, Duration::from_secs(30));
    assert_eq!(c.telemetry.service_name, "kaibo-env");
}

#[test]
fn env_can_disable_telemetry_that_the_file_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[telemetry]\nenabled = true\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_TELEMETRY_ENABLED", "0")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert!(
        !c.telemetry.enabled,
        "KAIBO_TELEMETRY_ENABLED=0 must turn off a file-enabled exporter"
    );
}

#[test]
fn a_non_numeric_telemetry_timeout_is_a_loud_error() {
    let env: HashMap<&str, &str> = [("KAIBO_TELEMETRY_TIMEOUT_SECS", "soon")]
        .into_iter()
        .collect();
    let err = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap_err();
    assert!(
        format!("{err:#}").contains("KAIBO_TELEMETRY_TIMEOUT_SECS"),
        "got: {err:#}"
    );
}

// --- [context] house-rules files ----------------------------------------------

/// With no `[context]` table, kaibo reads `AGENTS.md` by default (vendor-neutral,
/// opt-out) and no user files — the behavior an operator gets for free.
#[test]
fn context_defaults_to_agents_md_only() {
    let c = Config::builtin();
    assert_eq!(c.context.project_files, vec!["AGENTS.md".to_string()]);
    assert!(c.context.user_files.is_empty());
}

/// An explicit `[context]` table replaces both lists — including the canonical
/// "share my CLAUDE.md" shape the feature was built for.
#[test]
fn context_table_sets_project_and_user_files() {
    let c = Config::from_toml_str(
        r#"
        [context]
        project_files = ["AGENTS.md", "docs/CONVENTIONS.md"]
        user_files = ["~/.claude/CLAUDE.md"]
        "#,
    )
    .unwrap();
    assert_eq!(
        c.context.project_files,
        vec!["AGENTS.md".to_string(), "docs/CONVENTIONS.md".to_string()]
    );
    // user_files are tilde-expanded at merge so assemble does pure filesystem work.
    let user = &c.context.user_files;
    assert_eq!(user.len(), 1);
    assert!(
        !user[0].to_string_lossy().starts_with('~'),
        "~ must be expanded at merge, got: {}",
        user[0].display()
    );
    assert!(user[0].to_string_lossy().ends_with(".claude/CLAUDE.md"));
}

/// An explicit empty `project_files = []` is the opt-out: it turns off even the
/// AGENTS.md default, rather than being ignored as "unset".
#[test]
fn context_explicit_empty_project_files_opts_out_of_the_default() {
    let c = Config::from_toml_str(
        r#"
        [context]
        project_files = []
        "#,
    )
    .unwrap();
    assert!(
        c.context.project_files.is_empty(),
        "an explicit [] opts out of the AGENTS.md default, got: {:?}",
        c.context.project_files
    );
}

/// `KAIBO_PROJECT_FILES` / `KAIBO_USER_FILES` override the file layer, colon-
/// separated like PATH; and an empty value is the env-level opt-out.
#[test]
fn context_env_overrides_file_and_empty_opts_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[context]\nproject_files = [\"AGENTS.md\"]\nuser_files = [\"/from/file.md\"]\n",
    )
    .unwrap();

    // Env replaces both: a two-entry project list, an empty user list (opt-out).
    let env: HashMap<&str, &str> = [
        ("KAIBO_PROJECT_FILES", "A.md:sub/B.md"),
        ("KAIBO_USER_FILES", ""),
    ]
    .into_iter()
    .collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(
        c.context.project_files,
        vec!["A.md".to_string(), "sub/B.md".to_string()],
        "KAIBO_PROJECT_FILES (colon-separated) replaces the file layer"
    );
    assert!(
        c.context.user_files.is_empty(),
        "an empty KAIBO_USER_FILES opts out of the file's user_files, got: {:?}",
        c.context.user_files
    );
}

// --- [prompts] system-prompt overrides ----------------------------------------

/// No `[prompts]` table → every override is `None` (the built-in preambles run).
#[test]
fn prompts_default_to_no_overrides() {
    let c = Config::builtin();
    assert!(c.prompts.explorer.is_none());
    assert!(c.prompts.oneshot.is_none());
    assert!(c.prompts.consult.is_none());
}

/// A `[prompts]` table is parsed verbatim, per phase — including a multiline
/// triple-quoted prompt, the expected authoring shape.
#[test]
fn prompts_table_sets_per_phase_overrides() {
    let c = Config::from_toml_str(
        r#"
        [prompts]
        explorer = "You are a security auditor."
        consult = """
        You are a staff engineer.
        Prefer architectural answers.
        """
        "#,
    )
    .unwrap();
    assert_eq!(
        c.prompts.explorer.as_deref(),
        Some("You are a security auditor.")
    );
    assert!(c
        .prompts
        .consult
        .as_deref()
        .unwrap()
        .contains("staff engineer"));
    // An unset phase stays None — the built-in runs.
    assert!(c.prompts.oneshot.is_none());
}

/// An empty (or whitespace-only) override is a loud load error — a blank system
/// prompt is never intended, and silently running it would strip the role framing
/// with no signal. Remove the key to fall back to the built-in.
#[test]
fn an_empty_prompt_override_is_a_loud_error() {
    for value in ["\"\"", "\"   \""] {
        let toml = format!("[prompts]\nexplorer = {value}\n");
        let err = Config::from_toml_str(&toml)
            .expect_err(&format!("empty override {value} must be rejected"));
        let msg = format!("{err:#}");
        assert!(msg.contains("[prompts] explorer"), "names the key: {msg}");
        assert!(msg.contains("empty"), "explains why: {msg}");
    }
}

/// An unknown phase key is a typo, not a silently-ignored no-op — `deny_unknown_fields`.
#[test]
fn an_unknown_prompt_key_is_a_load_error() {
    let err = Config::from_toml_str("[prompts]\nsynth = \"oops, wrong key\"\n")
        .expect_err("an unknown [prompts] key must be a load error");
    assert!(format!("{err:#}").contains("synth"), "names the bad key");
}

/// A per-model `preamble` rides the cast's slot table, beside effort/thinking_style.
#[test]
fn a_slot_carries_a_per_model_preamble() {
    let c = Config::from_toml_str(
        r#"
        [casts.team]
        explorer = { backend = "openai", id = "Gemma-4-E4B-it", preamble = "You are a careful reader." }
        synth = "anthropic/claude-sonnet-4-6"
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("team").unwrap();
    assert_eq!(
        cast.require_slot(ModelRole::Explorer)
            .unwrap()
            .preamble
            .as_deref(),
        Some("You are a careful reader.")
    );
    // The string-form synth slot carries none.
    assert!(cast
        .require_slot(ModelRole::Synth)
        .unwrap()
        .preamble
        .is_none());
}

/// An empty slot preamble is a loud load error — same rule as `[prompts]`.
#[test]
fn an_empty_slot_preamble_is_a_loud_error() {
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = "claude-opus-4-8", preamble = "   " }
        "#,
    )
    .expect_err("a blank slot preamble must be rejected");
    assert!(format!("{err:#}").contains("preamble"), "names it: {err:#}");
}

// --- [orientation] static repo map --------------------------------------------

/// No `[orientation]` table → on by default, 256-file ceiling, depth-4 fallback.
#[test]
fn orientation_defaults_on_with_256_ceiling() {
    let c = Config::builtin();
    assert!(c.orientation.enabled);
    assert_eq!(c.orientation.full_list_max_files, 256);
    assert_eq!(c.orientation.tree_max_depth, 4);
}

/// The table tunes every knob.
#[test]
fn orientation_table_tunes_enabled_and_ceiling() {
    let c = Config::from_toml_str(
        "[orientation]\nenabled = false\nfull_list_max_files = 1000\ntree_max_depth = 6\n",
    )
    .unwrap();
    assert!(!c.orientation.enabled);
    assert_eq!(c.orientation.full_list_max_files, 1000);
    assert_eq!(c.orientation.tree_max_depth, 6);
}

/// A zero `tree_max_depth` is a loud load error — it would render an empty
/// directory map; disable instead.
#[test]
fn a_zero_tree_depth_is_a_loud_error() {
    let err = Config::from_toml_str("[orientation]\ntree_max_depth = 0\n")
        .expect_err("a zero depth must be rejected");
    assert!(
        format!("{err:#}").contains("tree_max_depth"),
        "names the knob: {err:#}"
    );
}

/// A zero ceiling is a loud load error — it would refuse every repo; disable
/// instead. (Same "a knob that silently does nothing is the failure we refuse"
/// discipline as the other limits.)
#[test]
fn a_zero_orientation_ceiling_is_a_loud_error() {
    let err = Config::from_toml_str("[orientation]\nfull_list_max_files = 0\n")
        .expect_err("a zero ceiling must be rejected");
    assert!(
        format!("{err:#}").contains("full_list_max_files"),
        "names the knob: {err:#}"
    );
}
