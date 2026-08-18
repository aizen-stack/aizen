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
    assert!(!p.sample_model.is_empty());
    // The preset used to tell the user to go and run `aizen auth login codex` themselves. It no
    // longer has to: `prompt_connection` recognises this base URL and offers the browser sign-in in
    // place of the API-key prompt, so the hint describes what happens rather than assigning homework.
    assert!(
        !p.keys_url.contains("auth login"),
        "the sign-in is offered inline now, not delegated to a second command: {}",
        p.keys_url
    );
}

/// Every preset must be usable as a saved provider profile without the user renaming it, and the
/// names must not collide — two presets defaulting to the same name would make the second one look
/// like it "already exists" for no reason the user can see.
#[test]
fn every_preset_offers_a_usable_profile_name() {
    let mut seen = std::collections::BTreeSet::new();
    for p in PROVIDER_PRESETS {
        assert!(
            cli_config::ProviderProfile::normalized(p.slug, p.base, "k", p.sample_model).is_ok(),
            "preset {} cannot be saved as typed (slug {:?}, base {:?})",
            p.label,
            p.slug,
            p.base
        );
        assert!(
            seen.insert(p.slug),
            "two presets both default to the name {:?}",
            p.slug
        );
    }
}

/// The free tag is the only warning a user gets before picking a paid model with a free-tier
/// credential — a mistake that surfaces as a 403 on the first real turn, long after the choice.
/// It regressed once by living in one picker's body instead of a shared row renderer.
#[test]
fn model_row_tags_free_tiers_from_either_signal() {
    let row = |id: &str, is_free: bool| {
        model_row(&client::ModelInfo {
            id: id.to_string(),
            context_length: None,
            is_free,
        })
    };
    // Pricing-derived (OpenRouter reports 0/0) and id-derived (OpenCode's `-free`, OpenRouter's
    // `:free`) both have to tag, because only one of the two is available depending on the gateway.
    assert!(row("anthropic/claude-3-haiku", true).contains("· free"));
    assert!(row("deepseek-v4-flash-free", false).contains("· free"));
    assert!(row("meta-llama/llama-3.3-70b:free", false).contains("· free"));
    assert!(row("big-pickle", false).contains("· free"));
    // A paid id must NOT be tagged — a false "free" is worse than no tag at all.
    assert!(!row("gpt-4o", false).contains("free"));

    let ctx = model_row(&client::ModelInfo {
        id: "some-model-free".into(),
        context_length: Some(200_000),
        is_free: false,
    });
    assert!(ctx.contains("· free") && ctx.contains("200000 ctx"));
}

/// Codex has no `GET /models`, so its picker is fed from the shipped catalog. An empty catalog would
/// silently drop the user into manual id entry for ids the binary already knows.
#[test]
fn codex_models_come_from_the_shipped_catalog() {
    let infos = codex_model_infos();
    assert!(!infos.is_empty(), "the Codex catalog must not be empty");
    let ids: Vec<&str> = infos.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&crate::llm::codex_models::default_model()),
        "the default model {:?} is not in the catalog it is picked from",
        crate::llm::codex_models::default_model()
    );
    // Plan-priced, not a gateway's no-charge tier — tagging these `free` would be a lie.
    assert!(infos.iter().all(|m| !m.is_free));
    let preset = PROVIDER_PRESETS
        .iter()
        .find(|p| p.slug == "codex")
        .expect("codex preset");
    assert!(
        ids.contains(&preset.sample_model),
        "the preset's sample model {:?} is not one the picker offers",
        preset.sample_model
    );
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
        None,
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
    apply_role_flags(&mut roles, "oracle", None, None, None, None);
    assert_eq!(
        roles.oracle.as_ref().unwrap().model.as_deref(),
        Some("strong-model"),
        "passing no flags must not clear an existing setting"
    );

    // Empty strings clear individual fields; emptying them all drops the role entirely.
    apply_role_flags(&mut roles, "oracle", None, Some(String::new()), None, None);
    assert_eq!(roles.oracle.as_ref().unwrap().model, None, "model cleared");
    assert!(roles.oracle.is_some(), "base_url still holds the role open");
    apply_role_flags(&mut roles, "oracle", None, None, Some("  ".into()), None);
    assert!(
        roles.oracle.is_none(),
        "an all-empty role is removed, not left as a husk"
    );
    assert!(!roles.has_any());

    // Each role writes to its own slot.
    apply_role_flags(
        &mut roles,
        "summarizer",
        None,
        Some("cheap".into()),
        None,
        None,
    );
    apply_role_flags(&mut roles, "apply", None, Some("fast".into()), None, None);
    assert_eq!(roles.summarizer.unwrap().model.as_deref(), Some("cheap"));
    assert_eq!(roles.apply.unwrap().model.as_deref(), Some("fast"));
    assert!(roles.oracle.is_none() && roles.subagent_default.is_none());
}
/// The profile route (`roles.<role>.provider`) holds a role open exactly as the per-field
/// overrides do, and clears the same way — it is the spelling core's own struct calls preferred,
/// so it must not be the one with weaker guarantees.
#[test]
fn a_role_pinned_to_a_profile_sets_and_clears_like_any_other_field() {
    let mut roles = cli_config::RolesConfig::default();
    apply_role_flags(&mut roles, "oracle", Some("work".into()), None, None, None);
    assert_eq!(
        roles.oracle.as_ref().unwrap().provider.as_deref(),
        Some("work")
    );

    apply_role_flags(&mut roles, "oracle", Some(String::new()), None, None, None);
    assert!(
        roles.oracle.is_none(),
        "the profile was the role's only field, so clearing it drops the role"
    );

    // A per-role effort nobody can set from the CLI still counts as the role being configured:
    // clearing the fields we *can* see must not take a hand-written one down with it.
    let mut roles = cli_config::RolesConfig::default();
    roles.oracle = Some(cli_config::RoleModelConfig {
        model: Some("strong".into()),
        reasoning_effort: Some("xhigh".into()),
        ..Default::default()
    });
    apply_role_flags(&mut roles, "oracle", None, Some(String::new()), None, None);
    assert_eq!(
        roles.oracle.as_ref().unwrap().reasoning_effort.as_deref(),
        Some("xhigh"),
        "clearing the model must not discard the effort beside it"
    );
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
