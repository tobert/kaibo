//! Config loading: merge precedence, the two-`openai`-endpoints regression, and
//! the loud-failure invariants. Pure where possible — `from_toml_str` touches no
//! env or filesystem; the env/CLI layers are exercised through injectable seams.

use std::collections::HashMap;

use kaibo::config::{default_models, Config, Profile, ToolDisables};
use kaibo::credentials::{openai_base_url, ProviderKind, PLACEHOLDER_OPENAI_KEY};
use kaibo::server::ToolGating;

// --- Built-ins reproduce historical behavior ------------------------------

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
    assert_eq!(
        c.defaults.request_timeout,
        std::time::Duration::from_secs(900)
    );

    // The four built-in profiles, named after their kind, with the model ids that
    // used to live in `default_models`.
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::DeepSeek,
        ProviderKind::Gemini,
        ProviderKind::Openai,
    ] {
        let p = c.resolve_profile(kind.canonical_name()).unwrap();
        let (explorer, synth) = default_models(kind);
        assert_eq!(p.kind, kind);
        assert_eq!(p.explorer_model, explorer, "{kind:?} explorer");
        assert_eq!(p.synth_model, synth, "{kind:?} synth");
        // Tunables inherit the defaults.
        assert_eq!(p.max_tokens, 16384);
        assert_eq!(p.thinking_budget, 8192);
        assert_eq!(p.request_timeout, std::time::Duration::from_secs(900));
    }

    // Default provider is anthropic; no root unless configured.
    assert_eq!(c.default_provider, "anthropic");
    assert!(c.root.is_none());
    assert_eq!(c.tools, expect_all_tools());
}

fn expect_all_tools() -> ToolGating {
    ToolGating::default()
}

#[test]
fn builtin_aliases_resolve() {
    let c = Config::builtin();
    assert_eq!(c.resolve_profile("claude").unwrap().kind, ProviderKind::Anthropic);
    assert_eq!(c.resolve_profile("google").unwrap().kind, ProviderKind::Gemini);
    for a in ["local", "lemonade", "gemma", "gemma4"] {
        assert_eq!(
            c.resolve_profile(a).unwrap().kind,
            ProviderKind::Openai,
            "{a:?} should alias openai"
        );
    }
}

// --- The headline: two openai endpoints, both live -------------------------

#[test]
fn two_openai_profiles_resolve_to_distinct_endpoints() {
    // The regression that proves the enum-as-selector bug is fixed: one process,
    // two OpenAI-compatible backends, each selected by name with its own endpoint.
    let toml = r#"
        [profiles.gpt]
        kind = "openai"
        base_url = "https://api.openai.com/v1"
        explorer_model = "gpt-5-mini"
        synth_model = "gpt-5"

        [profiles.llama]
        kind = "openai"
        base_url = "http://localhost:8080/v1"
        explorer_model = "qwen2.5-coder-7b"
        synth_model = "qwen2.5-coder-32b"
    "#;
    let c = Config::from_toml_str(toml).unwrap();

    let gpt = c.resolve_profile("gpt").unwrap();
    let llama = c.resolve_profile("llama").unwrap();

    // The profile name is the TOML key.
    assert_eq!(gpt.name, "gpt");
    assert_eq!(llama.name, "llama");
    assert_eq!(gpt.kind, ProviderKind::Openai);
    assert_eq!(llama.kind, ProviderKind::Openai);
    assert_eq!(gpt.resolved_base_url(), "https://api.openai.com/v1");
    assert_eq!(llama.resolved_base_url(), "http://localhost:8080/v1");
    assert_ne!(gpt.resolved_base_url(), llama.resolved_base_url());
    assert_eq!(gpt.synth_model, "gpt-5");
    assert_eq!(llama.synth_model, "qwen2.5-coder-32b");

    // The four built-ins are still present alongside the new profiles.
    assert!(c.resolve_profile("anthropic").is_ok());
    assert!(c.resolve_profile("openai").is_ok());
}

// --- Merge precedence ------------------------------------------------------

#[test]
fn file_overrides_a_builtin_profile_field_only() {
    // Retarget just the openai synth model; everything else stays built-in.
    let c = Config::from_toml_str(
        r#"
        [profiles.openai]
        synth_model = "my-local-big-model"
        "#,
    )
    .unwrap();
    let p = c.resolve_profile("openai").unwrap();
    assert_eq!(p.synth_model, "my-local-big-model");
    // Untouched fields keep the built-in values. No explicit base_url → the
    // resolved URL is whatever OPENAI_BASE_URL/default yields (env-robust check).
    assert_eq!(p.explorer_model, "Gemma-4-E4B-it-GGUF");
    assert_eq!(p.resolved_base_url(), openai_base_url());
}

#[test]
fn per_profile_tunables_override_defaults_others_inherit() {
    let c = Config::from_toml_str(
        r#"
        [defaults]
        max_tokens = 20000
        thinking_budget = 9000

        [profiles.gpt]
        kind = "openai"
        base_url = "https://api.openai.com/v1"
        max_tokens = 32768
        thinking_budget = 16384
        "#,
    )
    .unwrap();

    // gpt overrides both tunables...
    let gpt = c.resolve_profile("gpt").unwrap();
    assert_eq!(gpt.max_tokens, 32768);
    assert_eq!(gpt.thinking_budget, 16384);

    // ...while a profile that didn't override inherits the (file-set) defaults.
    let anthropic = c.resolve_profile("anthropic").unwrap();
    assert_eq!(anthropic.max_tokens, 20000);
    assert_eq!(anthropic.thinking_budget, 9000);
    assert_eq!(c.defaults.max_tokens, 20000);
}

#[test]
fn request_timeout_seeds_from_defaults_and_overrides_per_profile() {
    // A slow local model wants a longer leash than a hosted API; the seam is a
    // [defaults] seed that a profile may raise (or lower) on its own.
    let c = Config::from_toml_str(
        r#"
        [defaults]
        request_timeout_secs = 120

        [profiles.slowlocal]
        kind = "openai"
        base_url = "http://localhost:13305/api/v1"
        request_timeout_secs = 1800
        "#,
    )
    .unwrap();

    use std::time::Duration;
    // The file-set default reseeds every built-in profile...
    assert_eq!(c.defaults.request_timeout, Duration::from_secs(120));
    assert_eq!(
        c.resolve_profile("anthropic").unwrap().request_timeout,
        Duration::from_secs(120)
    );
    // ...while the profile that overrode it keeps its own deadline.
    assert_eq!(
        c.resolve_profile("slowlocal").unwrap().request_timeout,
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
    assert_eq!(c.defaults.request_timeout, std::time::Duration::from_secs(45));
    // And it reseeds profiles that didn't override.
    assert_eq!(
        c.resolve_profile("anthropic").unwrap().request_timeout,
        std::time::Duration::from_secs(45)
    );
}

#[test]
fn zero_request_timeout_is_rejected_loudly() {
    // A zero deadline times out every call instantly — a mistake, not a config. The
    // no-silent-fallback directive: crash at load, don't brick calls at runtime.
    let err = Config::from_toml_str(
        r#"
        [profiles.broken]
        kind = "openai"
        base_url = "http://localhost:1/v1"
        request_timeout_secs = 0
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("request_timeout_secs"),
        "got: {err:#}"
    );
}

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
fn new_profile_inherits_its_kinds_key_source_and_models() {
    // A second anthropic profile with no model overrides should inherit the kind's
    // built-in models and key source — convenient, not a forced re-spec.
    let c = Config::from_toml_str(
        r#"
        [profiles.work-claude]
        kind = "anthropic"
        "#,
    )
    .unwrap();
    let p = c.resolve_profile("work-claude").unwrap();
    let (explorer, synth) = default_models(ProviderKind::Anthropic);
    assert_eq!(p.explorer_model, explorer);
    assert_eq!(p.synth_model, synth);
    assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
}

#[test]
fn file_declared_aliases_resolve() {
    let c = Config::from_toml_str(
        r#"
        [profiles.big]
        kind = "openai"
        base_url = "http://localhost:9001/v1"
        aliases = ["heavy", "smart"]
        "#,
    )
    .unwrap();
    assert_eq!(c.resolve_profile("heavy").unwrap().name, "big");
    assert_eq!(c.resolve_profile("smart").unwrap().name, "big");
}

// --- Loud failures (crash over silent degrade) -----------------------------

#[test]
fn malformed_toml_is_an_error() {
    assert!(Config::from_toml_str("this is not = = valid toml").is_err());
}

#[test]
fn an_unknown_key_is_rejected() {
    // deny_unknown_fields: a typo'd knob must fail loudly, not silently no-op.
    let err = Config::from_toml_str(
        r#"
        [server]
        provder = "openai"
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("provder")
            || format!("{err:#}").to_lowercase().contains("unknown"),
        "got: {err:#}"
    );
}

#[test]
fn base_url_on_a_keyed_kind_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [profiles.weird]
        kind = "anthropic"
        base_url = "https://example.test/v1"
        "#,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("base_url"), "got: {err:#}");
}

#[test]
fn a_new_profile_without_a_kind_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [profiles.mystery]
        synth_model = "x"
        "#,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("kind"), "got: {err:#}");
}

#[test]
fn an_unknown_default_provider_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [server]
        provider = "does-not-exist"
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("does-not-exist"),
        "got: {err:#}"
    );
}

#[test]
fn an_alias_colliding_with_a_profile_name_is_rejected() {
    let err = Config::from_toml_str(
        r#"
        [profiles.foo]
        kind = "openai"
        base_url = "http://localhost:1/v1"
        aliases = ["anthropic"]
        "#,
    )
    .unwrap_err();
    assert!(format!("{err:#}").to_lowercase().contains("alias"), "got: {err:#}");
}

// --- File / env / CLI layering --------------------------------------------

#[test]
fn a_missing_default_path_yields_builtins_not_an_error() {
    // No file at the default location is fine — kaibo runs out of the box.
    let c = Config::load_with(
        None,
        Some("/nonexistent/kaibo/config.toml".into()),
        |_| None,
    )
    .expect("absent default config must not error");
    assert_eq!(c.default_provider, "anthropic");
    assert!(c.resolve_profile("openai").is_ok());
}

#[test]
fn a_missing_explicit_path_is_an_error() {
    // An explicit --config / KAIBO_CONFIG that doesn't exist is a mistake, loud.
    let err = Config::load_with(Some("/nonexistent/kaibo.toml".into()), None, |_| None)
        .unwrap_err();
    assert!(format!("{err:#}").contains("not found"), "got: {err:#}");
}

#[test]
fn env_overrides_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
        [server]
        provider = "openai"
        [defaults]
        max_tokens = 11111
        "#,
    )
    .unwrap();

    // Both values sit above the 8192 thinking budget, so the resulting anthropic
    // profile stays valid; the point under test is env-over-file precedence.
    let env: HashMap<&str, &str> = [
        ("KAIBO_PROVIDER", "anthropic"),
        ("KAIBO_MAX_TOKENS", "22222"),
    ]
    .into_iter()
    .collect();

    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    // env wins over the file.
    assert_eq!(c.default_provider, "anthropic");
    assert_eq!(c.defaults.max_tokens, 22222);
    // And the env'd default flows into a profile that inherits it.
    assert_eq!(c.resolve_profile("anthropic").unwrap().max_tokens, 22222);
}

#[test]
fn a_non_numeric_env_tunable_is_a_loud_error() {
    let env: HashMap<&str, &str> = [("KAIBO_MAX_TOKENS", "lots")].into_iter().collect();
    let err = Config::load_with(None, None, |k| env.get(k).map(|s| s.to_string())).unwrap_err();
    assert!(format!("{err:#}").contains("KAIBO_MAX_TOKENS"), "got: {err:#}");
}

#[test]
fn cli_overrides_win_over_everything() {
    let mut c = Config::builtin();
    c.apply_cli(
        Some("/tmp/proj".into()),
        Some("openai".to_string()),
        // Only --no-explore was passed.
        ToolDisables { explore: true, ..Default::default() },
    );
    assert_eq!(c.root.as_deref(), Some(std::path::Path::new("/tmp/proj")));
    assert_eq!(c.default_provider, "openai");
    // Only explore is dropped; the rest stay enabled.
    assert!(c.tools.consult);
    assert!(!c.tools.explore);
    assert!(c.tools.run_kaish);
}

// --- Key resolution --------------------------------------------------------

#[test]
fn key_optional_profile_falls_back_to_placeholder() {
    // A keyless profile whose env var is unset resolves to the placeholder, not an
    // error — the local-server case.
    let p = Profile {
        name: "local".into(),
        kind: ProviderKind::Openai,
        base_url: Some("http://localhost:1/v1".into()),
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: None,
        key_optional: true,
        explorer_model: "x".into(),
        synth_model: "y".into(),
        max_tokens: 16384,
        thinking_budget: 8192,
        request_timeout: std::time::Duration::from_secs(900),
    };
    assert_eq!(p.resolve_key().unwrap(), PLACEHOLDER_OPENAI_KEY);
}

#[test]
fn key_optional_profile_with_a_present_but_empty_key_file_errors() {
    // The no-silent-fallback invariant: a key file that's THERE but empty is a
    // mistake, not "keyless" — it must error, not quietly use the placeholder.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blank-key");
    std::fs::write(&file, "   \n").unwrap();
    let p = Profile {
        name: "local".into(),
        kind: ProviderKind::Openai,
        base_url: Some("http://localhost:1/v1".into()),
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: Some(file.to_string_lossy().into_owned()),
        key_optional: true,
        explorer_model: "x".into(),
        synth_model: "y".into(),
        max_tokens: 16384,
        thinking_budget: 8192,
        request_timeout: std::time::Duration::from_secs(900),
    };
    let err = p.resolve_key().unwrap_err();
    assert!(
        format!("{err:#}").contains("empty"),
        "a present-but-empty key file must error even when key_optional, got: {err:#}"
    );
}

// --- [sandbox] -------------------------------------------------------------

#[test]
fn sandbox_defaults_when_unconfigured() {
    let c = Config::builtin();
    assert_eq!(c.sandbox.exec_timeout, std::time::Duration::from_secs(30));
    assert_eq!(c.sandbox.output_limit_bytes, 8 * 1024);
    assert!(c.sandbox.disable_builtins.is_empty());
}

#[test]
fn sandbox_section_parses() {
    let c = Config::from_toml_str(
        r#"
        [sandbox]
        exec_timeout_secs = 5
        output_limit_bytes = 4096
        disable_builtins = ["rg", "find"]
        "#,
    )
    .unwrap();
    assert_eq!(c.sandbox.exec_timeout, std::time::Duration::from_secs(5));
    assert_eq!(c.sandbox.output_limit_bytes, 4096);
    assert_eq!(c.sandbox.disable_builtins, vec!["rg".to_string(), "find".to_string()]);
}

#[test]
fn sandbox_env_overrides_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[sandbox]\nexec_timeout_secs = 30\n").unwrap();
    let env: HashMap<&str, &str> = [("KAIBO_EXEC_TIMEOUT_SECS", "7")].into_iter().collect();
    let c = Config::load_with(None, Some(path), |k| env.get(k).map(|s| s.to_string())).unwrap();
    assert_eq!(c.sandbox.exec_timeout, std::time::Duration::from_secs(7));
}

#[test]
fn validate_against_builtins_rejects_an_unknown_name() {
    let c = Config::from_toml_str(
        r#"
        [sandbox]
        disable_builtins = ["rg", "definitely-not-a-builtin"]
        "#,
    )
    .unwrap();
    let known = vec!["rg".to_string(), "cat".to_string(), "find".to_string()];
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
        disable_builtins = ["rg"]
        "#,
    )
    .unwrap();
    let known = vec!["rg".to_string(), "cat".to_string()];
    assert!(c.validate_against_builtins(&known).is_ok());
}

#[test]
fn thinking_budget_at_or_above_max_tokens_is_rejected() {
    // Anthropic requires max_tokens > thinking_budget; catch the inverted config at
    // load, not as a runtime 400.
    let err = Config::from_toml_str(
        r#"
        [profiles.bad]
        kind = "anthropic"
        max_tokens = 4096
        thinking_budget = 8192
        "#,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("thinking_budget"),
        "got: {err:#}"
    );
}

#[test]
fn required_key_with_no_source_is_an_error() {
    let p = Profile {
        name: "needs-key".into(),
        kind: ProviderKind::Anthropic,
        base_url: None,
        api_key_env: Some("KAIBO_TEST_DEFINITELY_UNSET_KEY".into()),
        api_key_file: None,
        key_optional: false,
        explorer_model: "x".into(),
        synth_model: "y".into(),
        max_tokens: 16384,
        thinking_budget: 8192,
        request_timeout: std::time::Duration::from_secs(900),
    };
    assert!(p.resolve_key().is_err());
}
