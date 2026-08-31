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

    // Turn caps are set high on purpose (the synthesis agent rarely wastes turns and a
    // cap-hit now degrades gracefully rather than failing) — see Defaults::default.
    assert_eq!(c.defaults.explorer_max_turns, 100);
    assert_eq!(c.defaults.synth_max_turns, 200);
    // Token/thinking budgets still match the old ConsultConfig + THINKING_BUDGET.
    assert_eq!(c.defaults.max_tokens, 16384);
    assert_eq!(c.defaults.thinking_budget, 8192);
    // The per-request LLM deadline default: 15 min (see Defaults::default).
    assert_eq!(c.defaults.request_timeout, Duration::from_secs(900));

    // Five built-in backends + five single-backend casts, carrying the model ids
    // that used to live on the profiles. Named after their kind, except the OpenAI
    // built-in, which is `openai-local` (it defaults to a local endpoint). Every
    // kind in the registry is enumerated here, so silently dropping a built-in
    // fails this test.
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::OpenRouter,
        ProviderKind::Openai,
    ] {
        let name = kind.builtin_name();
        let backend = c.resolve_backend(name).unwrap();
        assert_eq!(backend.kind, kind);
        assert_eq!(backend.request_timeout, Duration::from_secs(900));

        let cast = c.resolve_cast(name).unwrap();
        let (explorer, synth) = default_models(kind);
        let e = cast.require_slot(ModelRole::Explorer).unwrap();
        let s = cast.require_slot(ModelRole::Synth).unwrap();
        assert_eq!(e.id, explorer, "{kind:?} explorer");
        assert_eq!(s.id, synth, "{kind:?} synth");
        // Built-in casts are single-backend, named by `builtin_name`.
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
            "openai-local",
            "{a:?} should alias the openai-local cast"
        );
    }
    // Backend level.
    assert_eq!(c.resolve_backend("claude").unwrap().name, "anthropic");
    assert_eq!(c.resolve_backend("google").unwrap().name, "gemini");
    for a in ["local", "lemonade", "gemma", "gemma4"] {
        assert_eq!(
            c.resolve_backend(a).unwrap().name,
            "openai-local",
            "{a:?} should alias the openai-local backend"
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
    // The use case the split exists for (docs/casts.md "Why"): a cheap local deepseek
    // explorer feeding a claude synth — one composed thing selected by one name.
    let c = Config::from_toml_str(
        r#"
        [casts.chimera]
        explorer = "deepseek/deepseek-v4-flash"
        synth = { backend = "claude", id = "claude-opus-4-8", effort = "max", max_tokens = 32768 }
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("chimera").unwrap();
    let e = cast.require_slot(ModelRole::Explorer).unwrap();
    let s = cast.require_slot(ModelRole::Synth).unwrap();

    // String form parses as backend/id; table form carries its tunables.
    assert_eq!(e.qualified(), "deepseek/deepseek-v4-flash");
    assert_eq!(s.qualified(), "anthropic/claude-opus-4-8");
    assert_eq!(s.effort.as_deref(), Some("max"));
    assert_eq!(s.max_tokens, Some(32768));

    // Two slots, two different backends — the fused profile could never say this.
    let backends: std::collections::BTreeSet<&str> =
        [e, s].iter().map(|slot| slot.backend.as_str()).collect();
    assert_eq!(backends.len(), 2, "each role on its own backend");
}

#[test]
fn slot_refs_split_on_the_first_slash_only() {
    // HuggingFace-style ids keep their inner slash: only the FIRST `/` splits.
    let (backend, id) = parse_slot_ref("openai-local/Qwen/Qwen3-32B").unwrap();
    assert_eq!(backend, "openai-local");
    assert_eq!(id, "Qwen/Qwen3-32B");
    // And the same through the TOML string form.
    let c = Config::from_toml_str(
        r#"
        [casts.hf]
        synth = "openai-local/Qwen/Qwen3-32B"
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
fn an_omitted_role_is_absent_and_named_loudly() {
    // Absent = capability absent, not an error (docs/casts.md): a cast may omit a
    // role, and require_slot names the gap loudly at call time.
    let c = Config::from_toml_str(
        r#"
        [casts.explore-only]
        explorer = "deepseek/deepseek-v4-flash"
        "#,
    )
    .unwrap();
    let cast = c.resolve_cast("explore-only").unwrap();
    assert!(cast.slot(ModelRole::Synth).is_none());
    let err = cast.require_slot(ModelRole::Synth).unwrap_err();
    assert!(
        format!("{err:#}").contains("has no synth slot"),
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
        explorer = { backend = "openai-local", id = "llava-13b", vision = true }
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
    assert!(c.resolve_cast("openai-local").is_ok());
}

#[test]
fn a_builtin_openai_backend_without_base_url_uses_the_env_default() {
    // No explicit base_url → the resolved URL is whatever OPENAI_BASE_URL/default
    // yields (env-robust check).
    let c = Config::builtin();
    let b = c.resolve_backend("openai-local").unwrap();
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

    // A built-in slot with no pin inherits the file-set defaults too (gemini's
    // synth carries no built-in max_tokens pin).
    let g = c
        .resolve_cast("gemini")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(g.max_tokens, 20000);
    assert_eq!(g.thinking_budget, 9000);
    assert_eq!(g.temperature, 0.5);

    // A built-in slot pin is an ordinary slot override, so it wins over the
    // file-set default the same way a user cast's pin does (the built-in
    // anthropic synth pins 32768 — see
    // `builtin_synth_max_tokens_pins_are_deliberate` in src/config.rs).
    let a = c
        .resolve_cast("anthropic")
        .unwrap()
        .require_slot(ModelRole::Synth)
        .unwrap()
        .tunables(ModelRole::Synth, &c.defaults);
    assert_eq!(a.max_tokens, 32768);
    assert_eq!(a.thinking_budget, 9000);
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
    for known in ["anthropic", "deepseek", "gemini", "openai-local"] {
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
    // rig fixes most keyed kinds' endpoints; a base_url there is a config mistake.
    // (Anthropic and gemini are the exceptions — see
    // `anthropic_backend_accepts_a_base_url` / `gemini_backend_accepts_a_base_url`.)
    let err = Config::from_toml_str(
        r#"
        [backends.deepseek]
        base_url = "https://example.test/v1"
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("base_url"), "got: {msg}");
    assert!(
        msg.contains(
            "only the `openai`, `anthropic`, and `gemini` completion kinds and the \
             media kinds"
        ),
        "got: {msg}"
    );
}

#[test]
fn anthropic_backend_accepts_a_base_url() {
    // Unlike the other keyed kinds, anthropic may point at a compatible
    // gateway/proxy (e.g. a corporate LLM gateway) instead of api.anthropic.com.
    let cfg = Config::from_toml_str(
        r#"
        [backends.anthropic]
        base_url = "https://example.test/v1"
        "#,
    )
    .unwrap();
    let b = cfg.backends.get("anthropic").unwrap();
    assert_eq!(b.base_url.as_deref(), Some("https://example.test/v1"));
}

#[test]
fn gemini_backend_accepts_a_base_url() {
    // Gemini may also point at a compatible gateway/proxy instead of
    // generativelanguage.googleapis.com — a HOST ROOT, same contract as anthropic's.
    let cfg = Config::from_toml_str(
        r#"
        [backends.gemini]
        base_url = "https://llm-gateway.example.internal"
        "#,
    )
    .unwrap();
    let b = cfg.backends.get("gemini").unwrap();
    assert_eq!(
        b.base_url.as_deref(),
        Some("https://llm-gateway.example.internal")
    );
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
    // `openai-local` backend keeps that fallback; a new stanza must say where it points.
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
    // The built-in `openai-local` backend keeps the env/default fallback: overriding
    // it without base_url stays valid, and a config-less load is unchanged.
    let c = Config::from_toml_str("[backends.openai-local]\nkey_optional = false\n").unwrap();
    assert!(c
        .resolve_backend("openai-local")
        .unwrap()
        .base_url
        .is_none());
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
    // The rule (Anthropic requires max_tokens > thinking_budget) binds only where the
    // model actually *sends* a budget; catch the inverted pair at load, not as a runtime
    // 400 — validated on the slot's RESOLVED values.

    // Per-slot override pair, inverted, on a budget-tier model (Haiku 4.5 sends
    // `budget_tokens`).
    let err = Config::from_toml_str(
        r#"
        [casts.x]
        synth = { backend = "anthropic", id = "claude-haiku-4-5", max_tokens = 1000, thinking_budget = 2000 }
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("thinking_budget (2000)"), "got: {msg}");
    assert!(msg.contains("max_tokens (1000)"), "got: {msg}");

    // The inversion can also arrive purely through [defaults]: a global max_tokens below
    // the default 8192 budget breaks the built-in budget-tier Anthropic slot (Haiku
    // explorer) at resolution time.
    let err = Config::from_toml_str("[defaults]\nmax_tokens = 4096\n").unwrap_err();
    assert!(
        format!("{err:#}").contains("thinking_budget"),
        "got: {err:#}"
    );

    // The same inverted pair is accepted where the model sends no budget: the generic
    // openai path (no thinking toggle), Gemini (takes a `thinkingLevel`), and Anthropic's
    // adaptive tier (takes an `output_config.effort`) all carry an inert `thinking_budget`.
    for (backend, id) in [
        ("openai-local", "m"),
        ("gemini", "gemini-3.5-flash"),
        ("anthropic", "claude-sonnet-4-6"),
    ] {
        Config::from_toml_str(&format!(
            "[casts.x]\nsynth = {{ backend = \"{backend}\", id = \"{id}\", max_tokens = 1000, thinking_budget = 2000 }}\n"
        ))
        .unwrap_or_else(|e| panic!("{backend}/{id} has no budget sink; the inverted pair must load: {e}"));
    }
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
        synth = { backend = "openai-local", id = "m", temperature = 3.0 }
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
        false,  // --no-cwd not passed
        vec![], // no --project-context-file flags
        vec![], // no --user-context-file flags
        false,  // --no-persistence not passed
        None,   // no --state-db
        None,   // --cas-dir
        None,   // --cas-max-bytes
        false,  // no --allow-save-artifact
        None,   // no --max-attachments
    );
    assert_eq!(c.default_cast, "deepseek", "--cast beats env and file");
    assert_eq!(c.root.as_deref(), Some(std::path::Path::new("/tmp/proj")));
    // Only oneshot is dropped; the rest stay enabled.
    assert!(c.tools.consult);
    assert!(c.tools.explore);
    assert!(c.tools.deliberate);
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
        false,
        vec![],
        vec![],
        false,
        None,
        None,  // --cas-dir
        None,  // --cas-max-bytes
        false, // --allow-save-artifact
        None,  // --max-attachments
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
    // And the env'd default flows into a slot that inherits it — gemini's synth,
    // which carries no built-in max_tokens pin (the anthropic and deepseek synths
    // do, and a slot pin wins over [defaults] from any layer; see
    // `builtin_synth_max_tokens_pins_are_deliberate` in src/config.rs).
    let t = c
        .resolve_cast("gemini")
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

#[test]
fn job_capacity_defaults_overrides_from_file_and_env() {
    use std::num::NonZeroUsize;
    // Absent → the built-in 64 (its own knob, smaller than sessions' 128).
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.defaults.job_capacity, NonZeroUsize::new(64).unwrap());
    // Set in [defaults] → honored, and independent of session_capacity.
    let c = Config::from_toml_str("[defaults]\njob_capacity = 9\n").unwrap();
    assert_eq!(c.defaults.job_capacity, NonZeroUsize::new(9).unwrap());
    assert_eq!(c.defaults.session_capacity, NonZeroUsize::new(128).unwrap());
    // Env wins over the file, like every other [defaults] knob.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[defaults]\njob_capacity = 9\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_JOB_CAPACITY", "5")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.job_capacity, NonZeroUsize::new(5).unwrap());
}

#[test]
fn zero_job_capacity_is_rejected_loudly() {
    // Same as sessions: a zero cap can't build an LruCache and would mean "hold no
    // jobs", defeating `consult_submit`. Fail at load, not on the first submit.
    let err = Config::from_toml_str("[defaults]\njob_capacity = 0\n").unwrap_err();
    assert!(format!("{err:#}").contains("job_capacity"), "got: {err:#}");
}

#[test]
fn inline_attach_budget_defaults_overrides_and_zero_is_legal() {
    // Absent → the built-in 256 KiB.
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.defaults.inline_attach_budget, 1 << 18);
    // Set in [defaults] → honored.
    let c = Config::from_toml_str("[defaults]\ninline_attach_budget = 4096\n").unwrap();
    assert_eq!(c.defaults.inline_attach_budget, 4096);
    // Zero is LEGAL (unlike the capacities): it means "inline nothing — demote every
    // text attachment to a read-WHOLE directive", the small-context escape hatch.
    let c = Config::from_toml_str("[defaults]\ninline_attach_budget = 0\n").unwrap();
    assert_eq!(c.defaults.inline_attach_budget, 0);
    // Env wins over the file, like every other [defaults] knob.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[defaults]\ninline_attach_budget = 4096\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_INLINE_ATTACH_BUDGET", "1024")]
        .into_iter()
        .collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.inline_attach_budget, 1024);
}

/// `max_attachments` bounds the explorer's `attach` tool (a sweep routing a file's
/// bytes past itself to the sweep's consumer) — its own knob from `inline_attach_budget`
/// (which bounds INLINING caller-supplied attachments into the driver prompt). Same
/// ladder shape: built-in default, `[defaults]` override, env wins over file, and `0`
/// is legal (it means "don't inject the attach tool at all").
#[test]
fn max_attachments_defaults_overrides_and_zero_is_legal() {
    // Absent → the built-in 32.
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.defaults.max_attachments, 32);
    // Set in [defaults] → honored.
    let c = Config::from_toml_str("[defaults]\nmax_attachments = 8\n").unwrap();
    assert_eq!(c.defaults.max_attachments, 8);
    // Zero is legal: it turns the attach tool off entirely.
    let c = Config::from_toml_str("[defaults]\nmax_attachments = 0\n").unwrap();
    assert_eq!(c.defaults.max_attachments, 0);
    // Env wins over the file, like every other [defaults] knob.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[defaults]\nmax_attachments = 8\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_MAX_ATTACHMENTS", "4")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.defaults.max_attachments, 4);
}

// --- Key resolution (now a Backend concern) --------------------------------------

fn local_backend(api_key_file: Option<String>, key_optional: bool) -> Backend {
    Backend {
        name: "local".into(),
        kind: ProviderKind::Openai,
        base_url: Some("http://localhost:1/v1".into()),
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file,
        api_key_cmd: None,
        key_optional,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
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
fn key_optional_gemini_backend_resolves_to_an_empty_query_key() {
    // Gemini authenticates with a `?key=` QUERY param, not a bearer header. A
    // keyless Gemini backend must resolve to the EMPTY string so rig emits a bare
    // `?key=` — which an ambient-auth gateway accepts — rather than the non-empty
    // "no-auth" placeholder, which such a gateway forwards to Google and Google
    // rejects (`API_KEY_INVALID`). Guards the keyless-gateway fix.
    let b = Backend {
        name: "gw-gemini".into(),
        kind: ProviderKind::Gemini,
        base_url: Some("http://gateway.internal".into()),
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: None,
        api_key_cmd: None,
        key_optional: true,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
    };
    assert_eq!(
        b.resolve_key().unwrap(),
        "",
        "a keyless Gemini backend must send an empty query key, not the bearer placeholder"
    );
}

#[test]
fn key_optional_non_gemini_backend_keeps_the_nonempty_placeholder() {
    // The other keyless kinds authenticate with a bearer/x-api-key HEADER, where an
    // empty value is rejected by some clients/servers — they must keep the non-empty
    // placeholder. Pins that the Gemini carve-out above didn't leak to header-auth kinds.
    let b = local_backend(None, true);
    assert_eq!(b.resolve_key().unwrap(), PLACEHOLDER_OPENAI_KEY);
    assert!(
        !b.resolve_key().unwrap().is_empty(),
        "header-auth keyless backends need a non-empty bearer placeholder"
    );
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
        api_key_cmd: None,
        key_optional: false,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
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
    // backends with the two agent roles.
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
    let slot = ModelSlot::bare("openai-local", "some-model");
    assert_eq!(slot.qualified(), "openai-local/some-model");
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
        explorer = { backend = "openai-local", id = "Gemma-4-E4B-it", preamble = "You are a careful reader." }
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

// --- Persistence layering ---------------------------------------------------
// `[persistence]` follows the same precedence as the rest of config: per-call/CLI >
// KAIBO_* env > file > built-in. On by default, at the XDG state db.

/// Default (no `[persistence]` table): enabled, at the XDG state-db path. Both the
/// `$XDG_STATE_HOME` and `~/.local/state` branches end in `kaibo/state.db`.
#[test]
fn persistence_defaults_on_at_the_xdg_state_db() {
    let c = Config::from_toml_str("").unwrap();
    assert!(c.persistence.enabled, "persistence is on by default");
    let path = c
        .persistence
        .path
        .expect("a default state-db path resolves");
    assert!(
        path.ends_with("kaibo/state.db"),
        "the default lands under the XDG state dir: {}",
        path.display()
    );
}

/// The file layer can turn it off and can point the db elsewhere (with `$VAR`/`~`
/// expansion, like `root`/`allow_paths`).
#[test]
fn persistence_file_layer_disables_and_repoints() {
    let off = Config::from_toml_str("[persistence]\nenabled = false\n").unwrap();
    assert!(
        !off.persistence.enabled,
        "enabled = false disables the store"
    );

    let home = std::env::var("HOME").expect("HOME set in test env");
    let repointed =
        Config::from_toml_str("[persistence]\npath = \"$HOME/kaibo-test.db\"\n").unwrap();
    assert_eq!(
        repointed.persistence.path.unwrap(),
        std::path::PathBuf::from(format!("{home}/kaibo-test.db")),
        "an explicit path is $VAR-expanded"
    );
}

/// The env layer: `KAIBO_NO_PERSISTENCE` disables, `KAIBO_STATE_DB` repoints — both over
/// the file layer.
#[test]
fn persistence_env_disables_and_overrides_path() {
    // KAIBO_NO_PERSISTENCE beats a file that enabled it.
    let env: HashMap<&str, &str> = [("KAIBO_NO_PERSISTENCE", "1")].into_iter().collect();
    let c = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert!(
        !c.persistence.enabled,
        "KAIBO_NO_PERSISTENCE disables the store"
    );

    // KAIBO_STATE_DB sets the path.
    let env: HashMap<&str, &str> = [("KAIBO_STATE_DB", "/var/lib/kaibo/s.db")]
        .into_iter()
        .collect();
    let c = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(
        c.persistence.path.unwrap(),
        std::path::PathBuf::from("/var/lib/kaibo/s.db"),
        "KAIBO_STATE_DB sets the state-db path"
    );
}

// --- the image role and the stability kind ----------------------------------

/// An `image` slot parses and lands on the cast. Until this change `image = …` was a
/// loud `deny_unknown_fields` error (986806f reduced the role set to explorer+synth);
/// reviving it is what lets a cast staff an artifact-producing tool.
#[test]
fn a_cast_can_carry_an_image_slot() {
    let c = Config::from_toml_str(
        r#"
        [backends.sd]
        kind = "stability"

        [casts.artist]
        explorer = "deepseek/deepseek-v4-flash"
        synth    = "deepseek/deepseek-v4-pro"
        image    = "sd/core"
        "#,
    )
    .expect("an image slot must parse");
    let cast = c.resolve_cast("artist").expect("cast resolves");
    let slot = cast
        .slot(kaibo::config::ModelRole::Image)
        .expect("the image slot is present");
    assert_eq!(slot.backend, "sd");
    assert_eq!(slot.id, "core");
}

/// A REASONING slot pointed at a Stability backend is a loud LOAD error. There is no
/// completion model behind an image API, so an arm built from one could only fail at
/// request time — and by then the operator is reading a provider error instead of being
/// told their config is wrong. Refuse at load, and name the fix.
#[test]
fn a_reasoning_slot_cannot_point_at_a_stability_backend() {
    for role in ["explorer", "synth"] {
        let toml = format!(
            r#"
            [backends.sd]
            kind = "stability"

            [casts.broken]
            {role} = "sd/core"
            "#
        );
        let err = Config::from_toml_str(&toml)
            .expect_err("a {role} slot on a stability backend must be refused at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(role) && msg.contains("stability"),
            "the error must name the offending slot and the kind, got: {msg}"
        );
        assert!(
            msg.contains("image"),
            "and it must point at the fix (the image slot), got: {msg}"
        );
    }
}

/// The mirror: an `image` slot on a chat backend is refused too. Asking a completion
/// model to return pixels fails just as surely, and just as confusingly, at request time.
#[test]
fn an_image_slot_cannot_point_at_a_completion_backend() {
    let err = Config::from_toml_str(
        r#"
        [casts.broken]
        image = "deepseek/deepseek-v4-pro"
        "#,
    )
    .expect_err("an image slot on a chat backend must be refused at load");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("image") && msg.contains("completion wire") && msg.contains("media-kind"),
        "the error names the slot, the offending class, and the class it needs — \
         never a kind list that goes stale, got: {msg}"
    );
}

// --- the openai-images kind ---------------------------------------------------

/// The `openai-images` kind parses from TOML. Declared-only: it carries NO key
/// source — the operator declares api_key_env / api_key_file / api_key_cmd (the
/// `openai` completion kind's conventions are what they'd likely name, but kaibo
/// seeds nothing) — and the key is REQUIRED by default: this kind's default endpoint
/// is hosted OpenAI, so a keyless seed would let a minimal stanza load clean, staff
/// `generate`, and then send `Bearer no-auth` to api.openai.com — a 401 on the
/// first paid call instead of a loud config gap (the or-gpt cross-family review's
/// posture reversal, 2026-08-03). A keyless local sd-server opts in with
/// `key_optional = true` beside its explicit local `base_url`.
#[test]
fn openai_images_parses_without_seeding_key_sources() {
    let c = Config::from_toml_str(
        r#"
        [backends.imgs]
        kind = "openai-images"
        "#,
    )
    .expect("the openai-images kind must parse");
    let b = c.backends.get("imgs").expect("backend exists");
    assert_eq!(b.kind, kaibo::credentials::ProviderKind::OpenAiImages);
    // Declared-only: a fresh media stanza declares its own source or fails loudly.
    assert_eq!(b.api_key_env, None);
    assert_eq!(b.api_key_file, None);
    assert_eq!(b.api_key_cmd, None);
    assert!(
        !b.key_optional,
        "key_optional seeds FALSE — the default endpoint is hosted, so a missing \
         key must fail loudly at resolve; a local sd-server opts in explicitly"
    );
}

/// base_url is optional on an openai-images backend, unlike a new `openai`
/// completion backend (which must say where it points): unset means hosted OpenAI
/// (`https://api.openai.com/v1`, resolved at arm construction), set points the same
/// wire at a local sd-server (which also sets `key_optional = true` — keyless is an
/// explicit opt-in on this kind). Both shapes load.
#[test]
fn openai_images_base_url_is_optional_default_hosted() {
    let c = Config::from_toml_str(
        r#"
        [backends.hosted-imgs]
        kind = "openai-images"

        [backends.sdcpp]
        kind = "openai-images"
        base_url = "http://localhost:1234/v1"
        key_optional = true
        "#,
    )
    .expect("both the hosted (no base_url) and local shapes load");
    assert_eq!(c.backends["hosted-imgs"].base_url, None);
    assert!(!c.backends["hosted-imgs"].key_optional);
    assert_eq!(
        c.backends["sdcpp"].base_url.as_deref(),
        Some("http://localhost:1234/v1")
    );
    assert!(c.backends["sdcpp"].key_optional);
}

/// An openai-images backend staffs an image slot — the config half of the
/// class-based pairing, exercised for the second media kind.
#[test]
fn an_image_slot_can_point_at_an_openai_images_backend() {
    let c = Config::from_toml_str(
        r#"
        [backends.imgs]
        kind = "openai-images"
        base_url = "http://localhost:1234/v1"

        [casts.artist]
        image = "imgs/sd3.5-large"
        "#,
    )
    .expect("an image slot on an openai-images backend loads");
    let cast = c.resolve_cast("artist").expect("cast resolves");
    let slot = cast
        .slot(kaibo::config::ModelRole::Image)
        .expect("the image slot is present");
    assert_eq!(slot.backend, "imgs");
    assert_eq!(slot.id, "sd3.5-large");
}

/// A reasoning slot pointed at an openai-images backend is refused at load with the
/// class-based error — proof the guard generalized instead of hardcoding stability.
#[test]
fn a_reasoning_slot_cannot_point_at_an_openai_images_backend() {
    for role in ["explorer", "synth"] {
        let toml = format!(
            r#"
            [backends.imgs]
            kind = "openai-images"

            [casts.broken]
            {role} = "imgs/gpt-image-1"
            "#
        );
        let err = Config::from_toml_str(&toml)
            .expect_err("a reasoning slot on an openai-images backend must be refused at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(role) && msg.contains("openai-images") && msg.contains("media kind"),
            "the error names the slot, the kind, and its class, got: {msg}"
        );
        assert!(
            msg.contains("image"),
            "and it points at the fix (the image slot), got: {msg}"
        );
    }
}

/// No BUILT-IN cast carries an image slot, so a stock install can staff no image tool —
/// the staffing gate then keeps that tool off the wire entirely, at zero resident cost
/// for every user who never configures one. Pins the premise that argument rests on.
#[test]
fn no_builtin_cast_carries_an_image_slot() {
    let c = Config::builtin();
    for (name, cast) in &c.casts {
        assert!(
            cast.slot(kaibo::config::ModelRole::Image).is_none(),
            "built-in cast {name:?} must not carry an image slot — image generation \
             costs real money per call and has to be configured deliberately"
        );
    }
}

/// Reasoning tunables WRITTEN on an image slot are diagnosed, not load errors: an image
/// slot sends one generation request with no reasoning phase, so a `thinking_budget` /
/// `effort` / `temperature` / `thinking_style` written there never reaches a request.
/// The diagnostic rides the same shared machinery as inert effort (startup warning +
/// `inert_tunables` in kaibo://config), and it names the cast, the slot, and each knob.
#[test]
fn written_reasoning_tunables_on_an_image_slot_are_diagnosed() {
    let c = Config::from_toml_str(
        r#"
        [backends.sd]
        kind = "stability"

        [casts.artist]
        explorer = "deepseek/deepseek-v4-flash"
        synth    = "deepseek/deepseek-v4-pro"
        image    = { backend = "sd", id = "core", thinking_budget = 4096, effort = "high", temperature = 0.5, thinking_style = "adaptive" }
        "#,
    )
    .expect("reasoning knobs on an image slot load fine; they are diagnosed, not refused");
    let diags = c.media_tunable_diagnostics();
    assert_eq!(diags.len(), 1, "exactly the one image slot is reported");
    let d = &diags[0];
    assert_eq!(d.cast, "artist");
    assert_eq!(d.role, "image");
    assert_eq!(d.model, "sd/core");
    assert_eq!(
        d.tunables,
        vec!["thinking_budget", "effort", "temperature", "thinking_style"],
        "every written reasoning knob is named, in render order"
    );
}

/// The quiet half: INHERITED defaults never flag an image slot. A `[defaults]`
/// synth-side effort states the synth posture and merely falls back onto other roles,
/// so a bare image slot stays silent in both scans — `media_tunable_diagnostics`
/// (nothing written on the slot) and `effort_diagnostics` (media roles are covered by
/// the media scan, so a defaults effort can't warn on every image slot the moment
/// someone sets `synth_effort`).
#[test]
fn inherited_defaults_stay_quiet_on_an_image_slot() {
    let c = Config::from_toml_str(
        r#"
        [defaults]
        synth_effort = "medium"

        [backends.sd]
        kind = "stability"

        [casts.artist]
        explorer = "deepseek/deepseek-v4-flash"
        synth    = "deepseek/deepseek-v4-pro"
        image    = "sd/core"
        "#,
    )
    .unwrap();
    assert!(
        c.media_tunable_diagnostics().is_empty(),
        "a bare image slot has nothing written, so nothing is reported"
    );
    assert!(
        !c.effort_diagnostics().iter().any(|d| d.role == "image"),
        "the effort scan covers reasoning roles only; an image slot inheriting a \
         defaults effort is a fallback artifact, not an operator statement"
    );
}

// --- [cas]: the media content-addressed store -------------------------------

/// The default posture: a dir under XDG *data* (not state — these are artifacts the user
/// paid for), and **no size cap**. The absent cap is the load-bearing default, not an
/// oversight: enforcing one means summing every file in the store on every write, over a
/// store that never deletes.
#[test]
fn cas_defaults_to_the_xdg_data_dir_and_no_cap() {
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(
        c.cas.max_bytes, None,
        "the CAS ships uncapped — a cap costs an O(objects) walk per write, so it is opt-in"
    );
    let dir = c
        .cas
        .dir
        .expect("a default CAS dir resolves from XDG_DATA_HOME or HOME");
    assert!(
        dir.ends_with("kaibo/cas"),
        "the default CAS dir is <data>/kaibo/cas, got {}",
        dir.display()
    );
    assert!(
        !dir.to_string_lossy().contains("/state/"),
        "the CAS belongs in the DATA dir, not beside the disposable state db: {}",
        dir.display()
    );
}

/// `[cas] enabled = false` is the explicit off switch (Amy, 2026-08-03); on is the
/// default, so an empty config runs with the CAS available.
#[test]
fn cas_enabled_defaults_true_and_the_file_can_turn_it_off() {
    let c = Config::from_toml_str("").unwrap();
    assert!(c.cas.enabled, "the CAS is on by default");
    let off = Config::from_toml_str("[cas]\nenabled = false\n").unwrap();
    assert!(!off.cas.enabled);
}

/// The one derivation of the CAS lifecycle: disabled wins over everything; otherwise
/// the CAS follows *runtime* persistence truth — disk while a durable store is open,
/// memory while it is not (including the degrade path where `[persistence]` is enabled
/// but the store failed to open) or when no directory resolves.
#[test]
fn cas_mode_follows_persistence_and_the_off_switch_wins() {
    use kaibo::config::CasMode;
    let c = Config::from_toml_str("").unwrap();
    assert_eq!(c.cas_mode(true), CasMode::Disk);
    assert_eq!(
        c.cas_mode(false),
        CasMode::Memory,
        "no durable persistence means an in-memory CAS, not a stranded disk store"
    );

    let off = Config::from_toml_str("[cas]\nenabled = false\n").unwrap();
    assert_eq!(off.cas_mode(true), CasMode::Off);
    assert_eq!(off.cas_mode(false), CasMode::Off);

    // No resolvable dir: enabled + persistence-active still cannot mean disk.
    let mut no_dir = Config::from_toml_str("").unwrap();
    no_dir.cas.dir = None;
    assert_eq!(no_dir.cas_mode(true), CasMode::Memory);
}

/// The file layer sets both knobs, and `$VAR`/`~` in `dir` expand like every other path.
#[test]
fn cas_file_layer_sets_dir_and_cap() {
    let c = Config::from_toml_str("[cas]\ndir = \"/srv/art\"\nmax_bytes = 1024\n").unwrap();
    assert_eq!(c.cas.dir.unwrap(), std::path::PathBuf::from("/srv/art"));
    assert_eq!(c.cas.max_bytes, Some(1024));
}

/// A zero ceiling is a loud load error, not a store that refuses every write. Nobody
/// means "refuse everything" by `max_bytes = 0`; accepting it would produce a CAS that
/// fails on first use with a capacity error no operator could explain.
#[test]
fn cas_zero_max_bytes_is_a_loud_load_error() {
    let err =
        Config::from_toml_str("[cas]\nmax_bytes = 0\n").expect_err("max_bytes = 0 must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("max_bytes") && msg.contains("> 0"),
        "the error must name the knob and the rule, got: {msg}"
    );
    assert!(
        msg.contains("Omit"),
        "and it must say how to get the uncapped default, got: {msg}"
    );
}

/// An unknown key under `[cas]` is refused — `deny_unknown_fields`, like every other
/// section, so a typo'd knob is never silently ignored.
#[test]
fn cas_unknown_key_is_refused() {
    assert!(
        Config::from_toml_str("[cas]\nmax_size = 10\n").is_err(),
        "a typo'd [cas] key must be a loud load error, not a silently-ignored knob"
    );
}

/// Env overrides the file, per the standard naming rule (`max_bytes` ⇄ KAIBO_CAS_MAX_BYTES).
#[test]
fn cas_env_overrides_the_file() {
    let env: HashMap<&str, &str> = [
        ("KAIBO_CAS_DIR", "/from/env"),
        ("KAIBO_CAS_MAX_BYTES", "4096"),
    ]
    .into_iter()
    .collect();
    let c = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.cas.dir.unwrap(), std::path::PathBuf::from("/from/env"));
    assert_eq!(c.cas.max_bytes, Some(4096));
}

/// The CLI is the top layer, and an ABSENT flag never clobbers a lower layer — the same
/// grammar every other optional flag here uses.
#[test]
fn cas_cli_wins_and_absent_flags_do_not_clobber() {
    let mut c = Config::from_toml_str("[cas]\ndir = \"/from/file\"\nmax_bytes = 111\n").unwrap();
    c.apply_cli(
        None,
        None,
        ToolDisables::default(),
        vec![],
        false,
        false,
        vec![],
        vec![],
        false,
        None,
        Some(std::path::PathBuf::from("/from/cli")), // --cas-dir
        None,                                        // --cas-max-bytes NOT passed
        false,                                       // --allow-save-artifact
        None,                                        // --max-attachments NOT passed
    );
    assert_eq!(
        c.cas.dir.unwrap(),
        std::path::PathBuf::from("/from/cli"),
        "--cas-dir wins over the file"
    );
    assert_eq!(
        c.cas.max_bytes,
        Some(111),
        "an absent --cas-max-bytes must leave the file's value alone, not reset it"
    );
}

// --- [artifacts]: kaibo's one inverted capability ----------------------------

/// **The default is OFF**, and that is the whole point of this stanza. Every other tool
/// switch is on unless an operator turns it off; this one is the reverse, because it is
/// the only surface where a *model* decides that bytes become durable. A regression here
/// would silently change the posture of every kaibo install.
#[test]
fn artifacts_are_disabled_by_default() {
    assert!(
        !Config::builtin().artifacts.enabled,
        "the built-in default must be off"
    );
    assert!(
        !Config::from_toml_str("").unwrap().artifacts.enabled,
        "an empty config file must not enable it either"
    );
    assert!(
        !Config::from_toml_str("[artifacts]\n")
            .unwrap()
            .artifacts
            .enabled,
        "an empty [artifacts] stanza is not a request to turn it on"
    );
}

#[test]
fn artifacts_enabled_in_the_file_turns_it_on() {
    assert!(
        Config::from_toml_str("[artifacts]\nenabled = true\n")
            .unwrap()
            .artifacts
            .enabled
    );
}

/// `deny_unknown_fields`, like every other section: a typo'd knob is a loud load error,
/// never a silently-ignored request to enable something.
#[test]
fn artifacts_unknown_key_is_refused() {
    assert!(Config::from_toml_str("[artifacts]\nenable = true\n").is_err());
}

/// Env can set the knob EITHER way (unlike the `KAIBO_NO_*` flags, which can only
/// disable) — because this knob's built-in default is off, so a layer that could only
/// disable would have nothing to say.
#[test]
fn artifacts_env_sets_the_knob_both_ways() {
    let with = |val: &str| {
        let env: HashMap<&str, &str> = [("KAIBO_ARTIFACTS_ENABLED", val)].into_iter().collect();
        Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string()))
            .unwrap()
            .artifacts
            .enabled
    };
    for on in ["1", "true", "yes", "TRUE", " Yes "] {
        assert!(with(on), "{on:?} must enable");
    }
    for off in ["0", "false", "no", "FALSE", " No "] {
        assert!(!with(off), "{off:?} must disable");
    }
}

/// **This one env flag refuses to guess.** kaibo's other env flags are permissive —
/// anything not empty/`0`/`false`/`no` means on — which for THIS flag would turn `off`,
/// `disabled`, and every typo into "let the model write durable bytes", the opposite of
/// what the operator wrote, silently. So it accepts only the listed spellings and fails
/// startup on anything else, naming them.
#[test]
fn artifacts_env_refuses_a_value_it_would_have_to_guess_at() {
    for bad in ["off", "disabled", "ture", "on", "", "2"] {
        let env: HashMap<&str, &str> = [("KAIBO_ARTIFACTS_ENABLED", bad)].into_iter().collect();
        let err = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string()))
            .expect_err(&format!("{bad:?} must be a loud load error, never a guess"));
        let msg = format!("{err:#}");
        assert!(
            msg.contains("KAIBO_ARTIFACTS_ENABLED"),
            "the error names the variable: {msg}"
        );
        assert!(
            msg.contains("true") && msg.contains("false"),
            "and the spellings it accepts: {msg}"
        );
    }
}

/// The CLI is the top layer here too, and it runs the OTHER way from `--no-<tool>`: the
/// flag enables. An absent flag never clobbers a lower layer that already turned it on.
#[test]
fn artifacts_cli_flag_enables_and_absence_does_not_clobber() {
    let apply = |mut c: Config, flag: bool| {
        c.apply_cli(
            None,
            None,
            ToolDisables::default(),
            vec![],
            false,
            false,
            vec![],
            vec![],
            false,
            None,
            None,
            None,
            flag, // --allow-save-artifact
            None, // --max-attachments
        );
        c.artifacts.enabled
    };
    assert!(
        apply(Config::from_toml_str("").unwrap(), true),
        "--allow-save-artifact turns it on over the off default"
    );
    assert!(
        apply(
            Config::from_toml_str("[artifacts]\nenabled = true\n").unwrap(),
            false
        ),
        "an absent flag must leave a file-enabled knob alone"
    );
    assert!(
        !apply(Config::from_toml_str("").unwrap(), false),
        "no flag and no file means off"
    );
}

/// The CLI is the top layer: `--no-persistence` and `--state-db` win over file/env.
#[test]
fn persistence_cli_wins_over_lower_layers() {
    // File enabled it at a path; CLI disables and repoints.
    let mut c = Config::from_toml_str("[persistence]\npath = \"/from/file.db\"\n").unwrap();
    assert!(c.persistence.enabled);
    c.apply_cli(
        None,
        None,
        ToolDisables::default(),
        vec![],
        false,
        false,
        vec![],
        vec![],
        true,                                           // --no-persistence
        Some(std::path::PathBuf::from("/from/cli.db")), // --state-db
        None,                                           // --cas-dir
        None,                                           // --cas-max-bytes
        false,                                          // no --allow-save-artifact
        None,                                           // no --max-attachments
    );
    assert!(!c.persistence.enabled, "--no-persistence wins");
    assert_eq!(
        c.persistence.path.unwrap(),
        std::path::PathBuf::from("/from/cli.db"),
        "--state-db wins over the file path"
    );
}

// --- the dashscope kind --------------------------------------------------------

/// The `dashscope` kind parses from TOML. Declared-only, like every other kind: the
/// operator names `api_key_env`/`api_key_file`/`api_key_cmd` themselves — kaibo
/// seeds nothing, not even DashScope's OWN conventional env var / key file names
/// (deliberately not shared with the `openai` completion kind, even though the same
/// DashScope host serves text on an OpenAI-compatible route — an operator running
/// both wires against one account wants one credential name per account, not one per
/// protocol). The key is required: this kind has no keyless target.
#[test]
fn dashscope_backend_parses_without_seeding_key_sources() {
    let c = Config::from_toml_str(
        r#"
        [backends.wan]
        kind = "dashscope"
        "#,
    )
    .expect("the dashscope kind must parse");
    let b = c.backends.get("wan").expect("backend exists");
    assert_eq!(b.kind, kaibo::credentials::ProviderKind::DashScope);
    // Declared-only: a fresh media stanza declares its own source or fails loudly.
    assert_eq!(b.api_key_env, None);
    assert_eq!(b.api_key_file, None);
    assert_eq!(b.api_key_cmd, None);
    assert!(
        !b.key_optional,
        "key_optional seeds FALSE — every DashScope endpoint is keyed"
    );
}

/// base_url is optional and legal on a dashscope backend: unset dials the shared
/// international root, and a dedicated-endpoint subscription sets its own host. The
/// client appends the route either way, so the value is a ROOT.
#[test]
fn dashscope_base_url_is_optional_and_legal() {
    let c = Config::from_toml_str(
        r#"
        [backends.shared]
        kind = "dashscope"

        [backends.dedicated]
        kind = "dashscope"
        base_url = "https://ws-example.us-east-1.maas.aliyuncs.com"
        "#,
    )
    .expect("a dashscope backend may set a base_url, and may omit it");
    assert!(
        c.backends.get("shared").expect("exists").base_url.is_none(),
        "unset means the kind's own default, resolved when the arm is staffed"
    );
    assert_eq!(
        c.backends
            .get("dedicated")
            .expect("exists")
            .base_url
            .as_deref(),
        Some("https://ws-example.us-east-1.maas.aliyuncs.com")
    );
}

/// A dashscope backend staffs an image slot — the config half of the media pairing.
#[test]
fn an_image_slot_can_point_at_a_dashscope_backend() {
    let c = Config::from_toml_str(
        r#"
        [backends.wan]
        kind = "dashscope"

        [casts.art]
        image = "wan/wan2.6-t2i"
        "#,
    )
    .expect("an image slot on a dashscope backend loads");
    let cast = c.casts.get("art").expect("cast exists");
    let slot = cast
        .slot(kaibo::config::ModelRole::Image)
        .expect("image slot resolves");
    assert_eq!(slot.backend, "wan");
    assert_eq!(slot.id, "wan2.6-t2i");
}

/// A reasoning slot pointed at a dashscope backend is refused at load — the
/// class-based guard, which is also the load-time answer to "why is my text model on
/// the wrong kind": DashScope text belongs on a `kind = "openai"` backend.
#[test]
fn a_reasoning_slot_cannot_point_at_a_dashscope_backend() {
    for role in ["explorer", "synth"] {
        let toml = format!(
            r#"
            [backends.wan]
            kind = "dashscope"

            [casts.broken]
            {role} = "wan/qwen3.8-max"
            "#
        );
        let err = Config::from_toml_str(&toml)
            .expect_err("a reasoning slot on a dashscope backend must be refused at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(role) && msg.contains("dashscope") && msg.contains("media kind"),
            "the error names the slot, the kind, and its class, got: {msg}"
        );
    }
}

// --- standard OTEL_* environment ------------------------------------------------

/// Helper: load with no config file and a fixed env map.
fn load_env(pairs: &[(&str, &str)]) -> Config {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Config::load_with(
        None,
        Some("/nonexistent/kaibo/config.toml".into()),
        move |k| {
            owned
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        },
    )
    .expect("config loads")
}

/// A collector in the environment turns telemetry ON by itself — the optimistic
/// enable. Safe only because content is redacted, which the same assertion pins.
#[test]
fn an_otlp_endpoint_in_the_environment_enables_redacted_telemetry() {
    let c = load_env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318")]);
    assert!(c.telemetry.enabled, "an OTLP endpoint is the opt-in signal");
    assert_eq!(
        c.telemetry.endpoint, "http://collector:4318/v1/traces",
        "the BASE var is a root; the signal path is appended"
    );
    assert!(
        !c.telemetry.capture_content,
        "turning on by ambient environment must never also turn on content"
    );
}

/// The per-signal var is a FULL url and is not suffixed — the other half of the
/// OTLP rule, and the half that would silently 404 if we got it backwards.
#[test]
fn the_per_signal_endpoint_is_used_verbatim_and_beats_the_base() {
    let c = load_env(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://base:4318"),
        (
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://traces:4318/custom/path",
        ),
    ]);
    assert_eq!(c.telemetry.endpoint, "http://traces:4318/custom/path");
}

/// A trailing slash on the base does not produce a doubled separator.
#[test]
fn a_trailing_slash_on_the_base_endpoint_is_tolerated() {
    let c = load_env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/")]);
    assert_eq!(c.telemetry.endpoint, "http://collector:4318/v1/traces");
}

/// The conventions' own content opt-in is honoured verbatim, so an operator who
/// already sets it for other instrumentations gets the same behaviour here.
#[test]
fn the_semconv_content_env_var_opts_in() {
    let c = load_env(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
        ("OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT", "true"),
    ]);
    assert!(c.telemetry.capture_content);
}

/// `OTEL_SDK_DISABLED` is a kill switch: it beats the endpoint that would have
/// enabled telemetry, and it beats KAIBO_TELEMETRY_ENABLED asking for it. A switch
/// something else can override is not a kill switch.
#[test]
fn otel_sdk_disabled_beats_everything() {
    let c = load_env(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
        ("KAIBO_TELEMETRY_ENABLED", "true"),
        ("OTEL_SDK_DISABLED", "true"),
    ]);
    assert!(
        !c.telemetry.enabled,
        "the standard kill switch has the last word"
    );
}

/// KAIBO_* beats OTEL_*: the kaibo-specific setting is the deliberate one, where
/// OTEL_* may have been set by a platform for its own purposes.
#[test]
fn kaibo_env_beats_the_ambient_otel_env() {
    let c = load_env(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://ambient:4318"),
        (
            "KAIBO_TELEMETRY_ENDPOINT",
            "http://deliberate:4318/v1/traces",
        ),
    ]);
    assert_eq!(c.telemetry.endpoint, "http://deliberate:4318/v1/traces");
}

/// An explicit `enabled = false` in the file is absolute — ambient environment may
/// supply what you left blank, never override what you wrote.
#[test]
fn an_explicit_file_disable_survives_an_ambient_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[telemetry]\nenabled = false\n").expect("write");
    let c = Config::load_with(Some(path), None, |k| {
        (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://collector:4318".to_string())
    })
    .expect("config loads");
    assert!(
        !c.telemetry.enabled,
        "a stated refusal must beat the ambient environment"
    );
}

/// With nothing in the environment, telemetry stays off — the optimistic enable is
/// triggered by a real endpoint, not by the feature existing.
#[test]
fn no_otel_environment_leaves_telemetry_off() {
    let c = load_env(&[]);
    assert!(!c.telemetry.enabled);
    assert!(!c.telemetry.capture_content);
}

// --- the gemini-images kind ------------------------------------------------------

/// The `gemini-images` kind parses. Declared-only, like every other kind: kaibo
/// seeds no key source, even though `gemini-images` **shares `gemini`'s conventional
/// env var and key-file names** (`credentials::ProviderKind::env_var`/
/// `key_file_name`) — one Google credential per account, not one per use of the
/// endpoint, so an operator who declares `api_key_env = "GEMINI_API_KEY"` on both
/// backends points them at the same key. That sharing is the difference from
/// `dashscope`, which keeps its own conventional names: there the two wires are
/// separate protocols against one host, whereas here they are literally the same
/// endpoint asked for a different modality. The key is required: the default target
/// is Google's hosted service, so a keyless seed would 401 on the first paid call
/// instead of failing at load.
#[test]
fn gemini_images_backend_parses_without_seeding_key_sources() {
    let c = Config::from_toml_str(
        r#"
        [backends.gimg]
        kind = "gemini-images"
        "#,
    )
    .expect("the gemini-images kind must parse");
    let b = c.backends.get("gimg").expect("backend exists");
    assert_eq!(b.kind, kaibo::credentials::ProviderKind::GeminiImages);
    // Declared-only: a fresh media stanza declares its own source or fails loudly.
    assert_eq!(b.api_key_env, None);
    assert_eq!(b.api_key_file, None);
    assert_eq!(b.api_key_cmd, None);
    assert!(
        !b.key_optional,
        "the default target is Google's hosted service, so the key is required"
    );
}

/// An `image` slot may point at it — the whole reason the kind exists.
#[test]
fn an_image_slot_can_point_at_a_gemini_images_backend() {
    let c = Config::from_toml_str(
        r#"
        [backends.gimg]
        kind = "gemini-images"

        [casts.painter]
        image = "gimg/gemini-3-flash-image"
        "#,
    )
    .expect("an image slot on a gemini-images backend is the supported pairing");
    assert!(c.casts.contains_key("painter"));
}

/// **A reasoning slot pointed at `gemini-images` is refused at load**, and this is the
/// pairing most likely to be got wrong: `gemini` and `gemini-images` are the same vendor
/// on the same endpoint, so an operator who types the wrong one has made a plausible
/// mistake rather than an obvious one. Text belongs on `kind = "gemini"`.
#[test]
fn a_reasoning_slot_cannot_point_at_a_gemini_images_backend() {
    for role in ["explorer", "synth"] {
        let toml = format!(
            r#"
            [backends.gimg]
            kind = "gemini-images"

            [casts.broken]
            {role} = "gimg/gemini-3-flash"
            "#
        );
        let err = Config::from_toml_str(&toml)
            .expect_err("a reasoning slot on a gemini-images backend must be refused at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(role) && msg.contains("gemini-images") && msg.contains("media kind"),
            "the error names the slot, the kind, and its class, got: {msg}"
        );
    }
}

// --- api_key_cmd: the declared command source --------------------------------
//
// Load-time rules (XOR, empty argv, no expansion) and resolve-time behavior
// (env wins without running it, cmd resolves on a keyless backend). The stub
// scripts are invoked by absolute path, Unix-only.

#[cfg(unix)]
fn cmd_stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A backend may declare a key file OR a key command — both is an ambiguity with
/// no sensible precedence, refused at load (the operator picks; env still overrides).
#[test]
fn file_and_cmd_declared_together_is_a_load_error() {
    let err = Config::from_toml_str(
        r#"
        [backends.vault]
        kind = "anthropic"
        api_key_file = "/some/key"
        api_key_cmd = ["op", "read", "op://Vault/Item"]
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("api_key_file") && msg.contains("api_key_cmd"),
        "the load error must name both declared sources, got: {msg}"
    );
}

/// An empty argv names no executable — a typo, not an intent.
#[test]
fn an_empty_api_key_cmd_is_a_load_error() {
    let err = Config::from_toml_str(
        r#"
        [backends.vault]
        kind = "anthropic"
        api_key_cmd = []
        "#,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("api_key_cmd") && msg.contains("executable"),
        "the load error must name the field and the fix, got: {msg}"
    );
}

/// Unlike `api_key_file`, a key command's argv is NEVER `$VAR`/`~`-expanded —
/// no shell, no interpolation, by design. A literal `$HOME` stays literal.
#[test]
fn api_key_cmd_argv_is_not_var_expanded() {
    let c = Config::from_toml_str(
        r#"
        [backends.vault]
        kind = "anthropic"
        api_key_cmd = ["$HOME/bin/op", "op://Vault/Item"]
        "#,
    )
    .unwrap();
    let b = c.resolve_backend("vault").unwrap();
    assert_eq!(
        b.api_key_cmd.as_deref(),
        Some(&["$HOME/bin/op".to_string(), "op://Vault/Item".to_string()][..]),
        "argv elements pass through load untouched — expansion is the command's own business"
    );
}

/// `key_status` reads a declared command as Present WITHOUT running it — there is
/// no cheap offline equivalent of `exists()` for a command, so a typo'd binary
/// name passes classification and fails loudly at the first resolve instead.
#[test]
fn key_status_reads_a_declared_cmd_as_present_without_running_it() {
    let b = Backend {
        name: "vault".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: None,
        api_key_cmd: Some(vec!["/nonexistent-kaibo-test/op".into()]),
        key_optional: false,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
    };
    assert_eq!(b.key_status(|_| None), kaibo::config::KeyStatus::Present);
}

/// The declared env source wins over the command WITHOUT running it — the
/// sentinel oracle: the stub touches a file only if it runs, so "env key came
/// back AND the sentinel is absent" proves the command never spawned. The
/// no-env arm is the negative control (the command runs then).
#[cfg(unix)]
#[test]
fn declared_env_wins_over_the_cmd_without_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("sentinel");
    let stub = cmd_stub(
        dir.path(),
        "op-stub",
        &format!("touch {}; printf 'cmd-key\\n'", sentinel.display()),
    );
    let b = Backend {
        name: "vault".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key_env: Some("KAIBO_TEST_CMD_ENV".into()),
        api_key_file: None,
        api_key_cmd: Some(vec![stub.to_string_lossy().into_owned()]),
        key_optional: false,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
    };

    // Env present: the injected lookup (the `resolve_key_where` seam, mirroring
    // `key_status`'s) wins, and the command never spawns.
    let key = b
        .resolve_key_where(|n| (n == "KAIBO_TEST_CMD_ENV").then(|| "env-key".into()))
        .expect("env source wins");
    assert_eq!(key, "env-key");
    assert!(
        !sentinel.exists(),
        "the command must not run when the env source resolves"
    );

    // Negative control: env absent → the command runs and its stdout is the key.
    let key = b.resolve_key_where(|_| None).expect("cmd source resolves");
    assert_eq!(
        key, "cmd-key",
        "the stub prints 'cmd-key' in the no-env case"
    );
    assert!(
        sentinel.exists(),
        "the no-env arm must actually run the command"
    );
}

/// A keyless backend can pull a real key from a command: the cmd arm sits before
/// the `key_optional` placeholder, so a declared command is used even when the
/// backend would otherwise fall back to keyless.
#[cfg(unix)]
#[test]
fn a_declared_key_cmd_resolves_even_on_a_key_optional_backend() {
    let dir = tempfile::tempdir().unwrap();
    let stub = cmd_stub(dir.path(), "op-stub", "printf 'sk-local\\n'");
    let b = Backend {
        name: "local".into(),
        kind: ProviderKind::Openai,
        base_url: Some("http://localhost:1/v1".into()),
        api_key_env: None,
        api_key_file: None,
        api_key_cmd: Some(vec![stub.to_string_lossy().into_owned()]),
        key_optional: true,
        request_timeout: Duration::from_secs(900),
        data_collection: Default::default(),
        wire: None,
    };
    let key = b.resolve_key().expect("cmd runs before the placeholder");
    assert_eq!(key, "sk-local");
}

/// The stream-isolation property at the binary level (the plan's MCP-stdio test):
/// a key command's stdout is captured AS THE KEY, never inherited into kaibo's own
/// stdout — which, in an MCP server, IS the protocol stream. The stub prints a
/// marker key; a real `oneshot` against a dead endpoint resolves the key (the
/// command runs), then fails on the transport — so a leaked child stdout would put
/// LEAK-KEY-123 in kaibo's stdout, and its absence alongside a transport failure
/// proves the pin. Child env is rebuilt from scratch (blank-env discipline); the
/// stub runs by absolute path, so no PATH is needed.
#[cfg(unix)]
#[test]
fn a_key_commands_stdout_never_reaches_kaibos_own_stdout() {
    let xdg = tempfile::tempdir().unwrap();
    let home = xdg.path().join("home");
    let config_home = xdg.path().join("config");
    std::fs::create_dir_all(config_home.join("kaibo")).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let stub = xdg.path().join("op-stub");
    std::fs::write(&stub, "#!/bin/sh\nprintf 'LEAK-KEY-123\\n'\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        config_home.join("kaibo/config.toml"),
        format!(
            "[backends.vault]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_cmd = [\"{}\"]\n\n[casts.vault-cast]\nsynth = \"vault/deepseek-v4-flash\"\n",
            stub.display()
        ),
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"p\"\n",
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_kaibo"))
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .env("XDG_DATA_HOME", xdg.path().join("data"))
        .args([
            "--root",
            project.path().to_str().unwrap(),
            "oneshot",
            "--cast",
            "vault-cast",
            "hi",
        ])
        .output()
        .expect("spawn the kaibo binary");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("LEAK-KEY-123"),
        "the key command's stdout must be captured, not inherited into kaibo's own \
         stdout:\n{stdout}"
    );
    assert!(
        stderr.to_lowercase().contains("refused")
            || stderr.to_lowercase().contains("failed")
            || stderr.to_lowercase().contains("error"),
        "the run must fail on the dead transport AFTER resolving the key (proving the \
         command ran):\n{stderr}"
    );

// --- the bfl kind ------------------------------------------------------------------

/// The `bfl` kind parses from TOML. Declared-only, like every other kind: the
/// operator names `api_key_env`/`api_key_file`/`api_key_cmd` themselves — kaibo
/// seeds nothing, not even BFL's own conventional `BFL_API_KEY` / `.bfl-key` names.
/// The key is required: like Stability and DashScope, this kind has no keyless
/// target.
#[test]
fn bfl_backend_parses_without_seeding_key_sources() {
    let c = Config::from_toml_str(
        r#"
        [backends.flux]
        kind = "bfl"
        "#,
    )
    .expect("the bfl kind must parse");
    let b = c.backends.get("flux").expect("backend exists");
    assert_eq!(b.kind, kaibo::credentials::ProviderKind::Bfl);
    // Declared-only: a fresh media stanza declares its own source or fails loudly.
    assert_eq!(b.api_key_env, None);
    assert_eq!(b.api_key_file, None);
    assert_eq!(b.api_key_cmd, None);
    assert!(!b.key_optional, "key_optional seeds FALSE — BFL is keyed");
}

/// base_url is optional and legal on a bfl backend: unset dials api.bfl.ai, and a
/// compatible gateway/proxy sets its own host. The client appends the op path
/// either way, so the value is a ROOT.
#[test]
fn bfl_base_url_is_optional_and_legal() {
    let c = Config::from_toml_str(
        r#"
        [backends.hosted]
        kind = "bfl"

        [backends.gateway]
        kind = "bfl"
        base_url = "https://llm-gateway.example.internal/bfl"
        "#,
    )
    .expect("a bfl backend may set a base_url, and may omit it");
    assert!(
        c.backends.get("hosted").expect("exists").base_url.is_none(),
        "unset means the kind's own default, resolved when the arm is staffed"
    );
    assert_eq!(
        c.backends
            .get("gateway")
            .expect("exists")
            .base_url
            .as_deref(),
        Some("https://llm-gateway.example.internal/bfl")
    );
}

/// A bfl backend staffs an image slot — the config half of the media pairing. The
/// slot's model id is one of BFL's named operations, and also `generate`'s default
/// `op`.
#[test]
fn an_image_slot_can_point_at_a_bfl_backend() {
    let c = Config::from_toml_str(
        r#"
        [backends.flux]
        kind = "bfl"

        [casts.art]
        image = "flux/flux-2-pro"
        "#,
    )
    .expect("an image slot on a bfl backend loads");
    let cast = c.casts.get("art").expect("cast exists");
    let slot = cast
        .slot(kaibo::config::ModelRole::Image)
        .expect("image slot resolves");
    assert_eq!(slot.backend, "flux");
    assert_eq!(slot.id, "flux-2-pro");
}

/// A reasoning slot pointed at a bfl backend is refused at load — the same
/// class-based guard every other media kind gets.
#[test]
fn a_reasoning_slot_cannot_point_at_a_bfl_backend() {
    for role in ["explorer", "synth"] {
        let toml = format!(
            r#"
            [backends.flux]
            kind = "bfl"

            [casts.broken]
            {role} = "flux/flux-2-pro"
            "#
        );
        let err = Config::from_toml_str(&toml)
            .expect_err("a reasoning slot on a bfl backend must be refused at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(role) && msg.contains("bfl") && msg.contains("media kind"),
            "the error names the slot, the kind, and its class, got: {msg}"
        );
    }
}
