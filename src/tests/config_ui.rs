//! Tests for the interactive configuration surface (`src/ui/config_ui.rs`).
//! They live beside their subject: a moved config editor should move its tests with it.

use super::*;

fn models() -> Vec<String> {
    vec![
        "opus-4-8".to_string(),
        "sonnet-4-6".to_string(),
        "minimax-m3".to_string(),
    ]
}

#[test]
fn opencode_free_preset_is_registered() {
    let p = PROVIDER_PRESETS
        .iter()
        .find(|p| p.label == "OpenCode (free)")
        .expect("the OpenCode free preset exists");
    assert_eq!(p.base, "https://opencode.ai/zen/v1");
    // The free tier's shared token is `public`; the hint must say so or a first-run user has no
    // way to guess the value the gateway expects.
    assert!(p.keys_url.contains("public"));
    assert!(
        p.sample_model.ends_with("-free"),
        "the sample must be a free-tier id, got {}",
        p.sample_model
    );
}
#[test]
fn codex_experimental_preset_is_registered() {
    let p = PROVIDER_PRESETS
        .iter()
        .find(|p| p.label == "ChatGPT Codex (experimental)")
        .expect("codex preset");
    assert_eq!(p.base, crate::llm::oauth_codex::CODEX_BASE_URL);
    assert!(p.keys_url.contains("auth login codex"));
    assert!(!p.sample_model.is_empty());
}
/// A role's key must never reach the screen in the clear.
///
/// The two shapes are deliberately different. `env:VAR` prints the VARIABLE NAME plus whether it
/// resolves right now — the failure this catches is a forgotten `export`, which otherwise looks
/// exactly like "the setting didn't save". A literal key goes through `mask()` like the main one.
#[test]
fn role_key_display_masks_literals_and_names_env_vars() {
    let literal = role_key_display(Some("sk-live-super-secret-value")).unwrap();
    assert!(
        !literal.contains("super-secret-value"),
        "a literal key must be masked, got {literal}"
    );
    assert!(
        literal.contains("***"),
        "masked form expected, got {literal}"
    );

    std::env::set_var("AIZEN_TEST_ROLE_KEY_SET", "some-value");
    let present = role_key_display(Some("env:AIZEN_TEST_ROLE_KEY_SET")).unwrap();
    assert!(
        present.contains("AIZEN_TEST_ROLE_KEY_SET"),
        "the variable NAME is the useful part, got {present}"
    );
    assert!(
        !present.contains("some-value"),
        "the variable's VALUE must never be printed, got {present}"
    );
    assert!(present.contains('✓'), "a resolvable var reads as ok");
    std::env::remove_var("AIZEN_TEST_ROLE_KEY_SET");

    std::env::remove_var("AIZEN_TEST_ROLE_KEY_UNSET");
    let missing = role_key_display(Some("env:AIZEN_TEST_ROLE_KEY_UNSET")).unwrap();
    assert!(
        missing.contains("unset"),
        "an unexported var must say so — that's the whole point, got {missing}"
    );

    assert_eq!(role_key_display(None), None);
    assert_eq!(role_key_display(Some("   ")), None);
}
#[test]
fn role_row_value_omits_absent_fields() {
    assert!(
        role_row_value(None).contains("not set"),
        "an unconfigured role says so"
    );
    // An all-empty struct is still "not set" — an empty husk shouldn't read as configured.
    assert!(role_row_value(Some(&cli_config::RoleModelConfig::default())).contains("not set"));

    let only_model = cli_config::RoleModelConfig {
        model: Some("cheap-fast".into()),
        ..Default::default()
    };
    let rendered = console::strip_ansi_codes(&role_row_value(Some(&only_model))).to_string();
    assert_eq!(
        rendered, "cheap-fast",
        "a model-only role is one word, not three 'not set' clauses"
    );
}
/// `--<role>-*` flags: `None` leaves a field alone, an EMPTY string clears it, and clearing the
/// last field drops the role object — which matters because `role_configured("oracle")` (and so
/// self-review) keys off presence, not contents.
#[test]
fn apply_role_flags_sets_clears_then_drops_the_role() {
    let mut roles = cli_config::RolesConfig::default();
    apply_role_flags(
        &mut roles,
        "oracle",
        Some("strong-model".into()),
        Some("https://oracle/v1".into()),
        None,
    );
    let o = roles.oracle.as_ref().expect("oracle materialized");
    assert_eq!(o.model.as_deref(), Some("strong-model"));
    assert_eq!(o.base_url.as_deref(), Some("https://oracle/v1"));
    assert_eq!(o.api_key_ref, None);
    assert!(roles.has_any());

    // A `None` for every field is a no-op, not a wipe.
    apply_role_flags(&mut roles, "oracle", None, None, None);
    assert_eq!(
        roles.oracle.as_ref().unwrap().model.as_deref(),
        Some("strong-model"),
        "passing no flags must not clear an existing setting"
    );

    // Empty strings clear individual fields; emptying them all drops the role entirely.
    apply_role_flags(&mut roles, "oracle", Some(String::new()), None, None);
    assert_eq!(roles.oracle.as_ref().unwrap().model, None, "model cleared");
    assert!(roles.oracle.is_some(), "base_url still holds the role open");
    apply_role_flags(&mut roles, "oracle", None, Some("  ".into()), None);
    assert!(
        roles.oracle.is_none(),
        "an all-empty role is removed, not left as a husk"
    );
    assert!(!roles.has_any());

    // Each role writes to its own slot.
    apply_role_flags(&mut roles, "summarizer", Some("cheap".into()), None, None);
    apply_role_flags(&mut roles, "apply", Some("fast".into()), None, None);
    assert_eq!(roles.summarizer.unwrap().model.as_deref(), Some("cheap"));
    assert_eq!(roles.apply.unwrap().model.as_deref(), Some("fast"));
    assert!(roles.oracle.is_none() && roles.subagent_default.is_none());
}
#[test]
fn version_suffix_hint_only_when_actually_missing() {
    for already in [
        "https://api.openai.com/v1",
        "https://api.openai.com/v1/",
        "https://api.groq.com/openai/v1",
        "http://localhost:11434/v1",
        "https://example.test/v2",
        "https://example.test/V3",
        // Google-style: a version with a trailing qualifier is still a version. Suggesting
        // `/v1beta/v1` would send the user to a path that certainly doesn't exist.
        "https://generativelanguage.googleapis.com/v1beta",
    ] {
        assert_eq!(
            missing_version_suffix(already),
            None,
            "{already} is already versioned"
        );
    }
    for (input, want) in [
        ("https://api.openai.com", "https://api.openai.com/v1"),
        ("https://api.openai.com/", "https://api.openai.com/v1"),
        (
            "https://api.groq.com/openai",
            "https://api.groq.com/openai/v1",
        ),
        ("http://localhost:11434", "http://localhost:11434/v1"),
        // `v` with no digits is a path segment, not a version.
        ("https://example.test/v", "https://example.test/v/v1"),
        // `vN` needs the digit FIRST — `vbeta` is just a path.
        (
            "https://example.test/vbeta",
            "https://example.test/vbeta/v1",
        ),
    ] {
        assert_eq!(
            missing_version_suffix(input).as_deref(),
            Some(want),
            "{input} should be offered {want}"
        );
    }
}
#[test]
fn default_index_points_at_the_saved_model() {
    assert_eq!(model_default_index(&models(), Some("sonnet-4-6")), 1);
    assert_eq!(model_default_index(&models(), Some("minimax-m3")), 2);
}
#[test]
fn default_index_falls_back_to_zero() {
    // No saved model, or a saved model the provider no longer lists → highlight the first.
    assert_eq!(model_default_index(&models(), None), 0);
    assert_eq!(model_default_index(&models(), Some("retired-model")), 0);
}
