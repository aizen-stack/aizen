//! The interactive **configuration surface**: `aizen config` / `aizen auth` and every editor the
//! `/config` menu opens.
//!
//! This is presentation only. The persisted shape and its defaults belong to
//! [`crate::core::cli_config`]; nothing here decides what a setting MEANS, only how it is shown,
//! prompted for, and validated before being handed back to the store. Keeping the two apart is the
//! reason a new setting is a small edit in one file instead of a hunt through the REPL.

use crate::cli_args::{AuthCmd, ConfigCmd, ProviderConfigCmd};
use crate::core::approval::ApprovalMode;
use crate::core::cli_config;
use crate::llm::client;
use crate::memory;
use crate::ui::{icons, splash, theme, tui};
use crate::{effective_ctx_window, http_client, ui_theme};
use anyhow::{Context, Result};
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

fn parse_toolset_list(s: &str) -> Option<Vec<String>> {
    let mut values: Vec<String> = s
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect();
    values.sort();
    values.dedup();
    (!values.is_empty()).then_some(values)
}

/// Apply one `--model-endpoint` spec to the config's model→endpoint registry. Spec is
/// `model[,base_url=URL][,api_key_ref=env:VAR|KEY]`. The first comma-token is the model id; the
/// rest are `key=value` fields. A bare model id (no fields) or a `clear` token removes the entry.
/// Upserts by exact model id.
fn apply_model_endpoint(cfg: &mut cli_config::CliConfig, spec: &str) -> Result<()> {
    let mut parts = spec.split(',').map(str::trim).filter(|s| !s.is_empty());
    let model = parts
        .next()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .context("--model-endpoint needs a model id (e.g. `gpt-4o,base_url=https://…`)")?;
    let mut base_url = None;
    let mut api_key_ref = None;
    let mut clear = false;
    for tok in parts {
        if tok.eq_ignore_ascii_case("clear") {
            clear = true;
            continue;
        }
        match tok.split_once('=') {
            Some(("base_url", v)) => base_url = Some(v.trim().to_string()),
            Some(("api_key_ref", v)) => api_key_ref = Some(v.trim().to_string()),
            _ => anyhow::bail!(
                "--model-endpoint field '{tok}' not understood (use base_url=… , api_key_ref=… , or clear)"
            ),
        }
    }
    let mut list = cfg.model_endpoints.take().unwrap_or_default();
    list.retain(|e| e.model != model);
    // A bare model id (no fields) or an explicit `clear` removes the entry (retain above already
    // dropped it); otherwise upsert the new mapping.
    if !clear && (base_url.is_some() || api_key_ref.is_some()) {
        list.push(cli_config::ModelEndpoint {
            model,
            base_url,
            api_key_ref,
        });
    }
    cfg.model_endpoints = (!list.is_empty()).then_some(list);
    Ok(())
}

/// Apply one role's `--<role>-model/-base-url/-api-key-ref` trio to `roles`.
///
/// `None` leaves a field alone; an EMPTY string clears it (the documented way to undo a setting
/// without hand-editing JSON). When every field of the role ends up cleared, the role object itself
/// is dropped so the saved config doesn't accumulate `"oracle": {}` husks — which matters beyond
/// tidiness, because `role_configured("oracle")` (and therefore self-review) keys off presence.
fn apply_role_flags(
    roles: &mut cli_config::RolesConfig,
    role: &str,
    model: Option<String>,
    base_url: Option<String>,
    api_key_ref: Option<String>,
) {
    if model.is_none() && base_url.is_none() && api_key_ref.is_none() {
        return;
    }
    let slot = match role {
        "summarizer" => &mut roles.summarizer,
        "subagent_default" => &mut roles.subagent_default,
        "oracle" => &mut roles.oracle,
        "apply" => &mut roles.apply,
        _ => return,
    };
    let mut rc = slot.take().unwrap_or_default();
    let set = |field: &mut Option<String>, v: Option<String>| {
        if let Some(s) = v {
            let s = s.trim();
            *field = (!s.is_empty()).then(|| s.to_string());
        }
    };
    set(&mut rc.model, model);
    set(&mut rc.base_url, base_url);
    set(&mut rc.api_key_ref, api_key_ref);
    *slot = (rc.model.is_some() || rc.base_url.is_some() || rc.api_key_ref.is_some()).then_some(rc);
}

pub(crate) async fn run_auth(cmd: AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Login { provider } => {
            let p = provider.trim().to_ascii_lowercase();
            if p != "codex" && p != "chatgpt" && p != "chatgpt-codex" {
                anyhow::bail!(
                    "unknown auth provider '{provider}' — supported: codex (ChatGPT Codex OAuth, experimental)"
                );
            }
            if crate::llm::oauth_codex::codex_disabled() {
                anyhow::bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
            }
            println!("{}", crate::llm::oauth_codex::risk_notice());
            println!();
            let set = crate::llm::oauth_codex::login_interactive().await?;
            let who = set.label.as_deref().unwrap_or("(signed in)");
            println!("✓ Codex login saved for {who}");
            println!(
                "  token file: {}",
                crate::llm::oauth_codex::token_path().display()
            );
            println!("  next: aizen config  → pick 'ChatGPT Codex (experimental)' or:");
            println!(
                "        aizen config set --base-url {} --api-key codex-oauth --model {}",
                crate::llm::oauth_codex::CODEX_BASE_URL,
                crate::llm::codex_models::default_model()
            );
            Ok(())
        }
        AuthCmd::Status => {
            for line in crate::llm::oauth_codex::status_lines() {
                println!("{line}");
            }
            Ok(())
        }
        AuthCmd::Logout { provider } => {
            let p = provider.trim().to_ascii_lowercase();
            if p != "codex" && p != "chatgpt" && p != "chatgpt-codex" {
                anyhow::bail!("unknown auth provider '{provider}' — supported: codex");
            }
            crate::llm::oauth_codex::clear_token();
            println!("✓ Codex tokens removed");
            Ok(())
        }
    }
}

pub(crate) async fn run_config(cmd: Option<ConfigCmd>) -> Result<()> {
    let cmd = match cmd {
        Some(c) => c,
        None => return config_wizard().await, // bare `aizen config` → interactive setup
    };
    match cmd {
        ConfigCmd::Set {
            base_url,
            api_key,
            model,
            context_window,
            compact_threshold,
            auto_skill_learn,
            memory_auto_learn,
            persona_evolve,
            price_in,
            price_out,
            icons,
            response_visuals,
            timemachine_keep,
            timemachine_max_files,
            timemachine_max_bytes,
            timemachine_max_file_bytes,
            auto_effort,
            reasoning_effort,
            approval,
            ultimate,
            adaptive_effort,
            disabled_toolsets,
            enabled_toolsets,
            subagent_model,
            subagent_base_url,
            subagent_api_key_ref,
            summarizer_model,
            summarizer_base_url,
            summarizer_api_key_ref,
            oracle_model,
            oracle_base_url,
            oracle_api_key_ref,
            apply_model,
            apply_base_url,
            apply_api_key_ref,
            model_endpoint,
        } => {
            // (role, model, base_url, api_key_ref) — one table so the emptiness guard below and the
            // apply loop further down can never disagree about which flags exist.
            let role_flags = [
                (
                    "subagent_default",
                    subagent_model,
                    subagent_base_url,
                    subagent_api_key_ref,
                ),
                (
                    "summarizer",
                    summarizer_model,
                    summarizer_base_url,
                    summarizer_api_key_ref,
                ),
                ("oracle", oracle_model, oracle_base_url, oracle_api_key_ref),
                ("apply", apply_model, apply_base_url, apply_api_key_ref),
            ];
            let any_role_flag = role_flags
                .iter()
                .any(|(_, m, b, k)| m.is_some() || b.is_some() || k.is_some());
            if base_url.is_none()
                && api_key.is_none()
                && model.is_none()
                && context_window.is_none()
                && compact_threshold.is_none()
                && auto_skill_learn.is_none()
                && memory_auto_learn.is_none()
                && persona_evolve.is_none()
                && price_in.is_none()
                && price_out.is_none()
                && icons.is_none()
                && response_visuals.is_none()
                && timemachine_keep.is_none()
                && timemachine_max_files.is_none()
                && timemachine_max_bytes.is_none()
                && timemachine_max_file_bytes.is_none()
                && auto_effort.is_none()
                && reasoning_effort.is_none()
                && approval.is_none()
                && ultimate.is_none()
                && adaptive_effort.is_none()
                && disabled_toolsets.is_none()
                && enabled_toolsets.is_none()
                && !any_role_flag
                && model_endpoint.is_empty()
            {
                anyhow::bail!("nothing to set — pass at least one supported --flag (including --timemachine-keep / --timemachine-max-files / --timemachine-max-bytes / --timemachine-max-file-bytes)");
            }
            let mut cfg = cli_config::load();
            let previous_url = cfg.base_url.clone();
            let base_url_was_set = base_url.is_some();
            if let Some(v) = base_url {
                cfg.base_url = Some(v.trim().trim_end_matches('/').to_string());
            }
            if let Some(v) = api_key {
                cfg.api_key = Some(v.trim().to_string());
            }
            if let Some(v) = model {
                cfg.model = Some(v.trim().to_string());
                cfg.model_context_window = None; // model changed manually → re-derive via heuristic
            }
            // An explicit --context-window wins (applied after model so it isn't cleared above).
            if let Some(w) = context_window {
                cfg.model_context_window = if w > 0 { Some(w) } else { None };
            }
            if let Some(t) = compact_threshold {
                if t != 0 && !(10..=95).contains(&t) {
                    anyhow::bail!("--compact-threshold must be 0 (off) or 10–95");
                }
                cfg.compact_threshold_pct = Some(t);
            }
            if let Some(b) = auto_skill_learn {
                cfg.auto_skill_learn = Some(b);
            }
            if let Some(b) = memory_auto_learn {
                cfg.memory_auto_learn = Some(b);
            }
            if let Some(b) = persona_evolve {
                cfg.persona_evolve = Some(b);
            }
            if let Some(p) = price_in {
                if p < 0.0 {
                    anyhow::bail!("--price-in must be ≥ 0");
                }
                cfg.price_in = Some(p);
            }
            if let Some(p) = price_out {
                if p < 0.0 {
                    anyhow::bail!("--price-out must be ≥ 0");
                }
                cfg.price_out = Some(p);
            }
            if let Some(v) = icons {
                let v = v.trim().to_ascii_lowercase();
                if !["emoji", "nerd", "off"].contains(&v.as_str()) {
                    anyhow::bail!("--icons must be one of: emoji, nerd, off");
                }
                cfg.icons = Some(v);
            }
            if let Some(v) = response_visuals {
                cfg.response_visuals = Some(
                    v.parse::<cli_config::ResponseVisuals>()
                        .map_err(anyhow::Error::msg)?,
                );
            }
            if let Some(k) = timemachine_keep {
                cfg.timemachine_keep = Some(k); // 0 = unlimited
            }
            if let Some(k) = timemachine_max_files {
                cfg.timemachine_max_files = Some(k.max(1));
            }
            if let Some(k) = timemachine_max_bytes {
                cfg.timemachine_max_bytes = Some(k.max(1));
            }
            if let Some(k) = timemachine_max_file_bytes {
                cfg.timemachine_max_file_bytes = Some(k.max(1));
            }
            if let Some(b) = auto_effort {
                cfg.auto_effort = Some(b);
            }
            if let Some(v) = reasoning_effort {
                let v = v.trim().to_ascii_lowercase();
                if !["low", "medium", "high", "xhigh", "max"].contains(&v.as_str()) {
                    anyhow::bail!(
                        "--reasoning-effort must be one of: low, medium, high, xhigh, max"
                    );
                }
                cfg.reasoning_effort = Some(v);
            }
            if let Some(v) = approval {
                cfg.set_approval_mode(v.parse::<ApprovalMode>().map_err(anyhow::Error::msg)?);
            }
            if let Some(b) = ultimate {
                cfg.ultimate = Some(b);
            }
            if let Some(b) = adaptive_effort {
                cfg.adaptive_effort = Some(b);
            }
            if let Some(v) = disabled_toolsets {
                cfg.disabled_toolsets = parse_toolset_list(&v);
            }
            if let Some(v) = enabled_toolsets {
                cfg.enabled_toolsets = parse_toolset_list(&v);
            }
            // Per-role endpoints (`roles.*`): set any of model/base_url/api_key_ref; an empty string
            // CLEARS that field. Editing any sub-field materializes the `roles` object; clearing
            // every field of every role drops it again.
            if any_role_flag {
                let mut roles = cfg.roles.take().unwrap_or_default();
                for (role, model, base_url, api_key_ref) in role_flags {
                    apply_role_flags(&mut roles, role, model, base_url, api_key_ref);
                }
                cfg.roles = roles.has_any().then_some(roles);
            }
            // Model→endpoint registry: each `--model-endpoint` is `model[,base_url=URL][,api_key_ref=…]`;
            // a bare model id or `model,clear` removes the entry.
            for spec in model_endpoint {
                apply_model_endpoint(&mut cfg, &spec)?;
            }
            if base_url_was_set {
                cfg.detach_provider_if_url_changed(previous_url.as_deref());
            }
            cfg.sync_active_provider();
            cli_config::save(&cfg)?;
            println!(
                "{} {}",
                crate::ui::theme::ok("✓"),
                style("saved").color256(splash::ACCENT)
            );
            print_config(&cfg);
            Ok(())
        }
        ConfigCmd::Provider { cmd } => run_provider_config(cmd),
        ConfigCmd::Show => {
            print_config(&cli_config::load());
            Ok(())
        }
        ConfigCmd::Path => {
            println!("{}", cli_config::config_path().display());
            Ok(())
        }
    }
}

pub(crate) fn provider_row(cfg: &cli_config::CliConfig, p: &cli_config::ProviderProfile) -> String {
    let active = cfg
        .active_provider
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(&p.name));
    format!(
        "{} {:<16} · {} · {} · key {}",
        if active { "●" } else { "○" },
        p.name,
        p.model,
        redact_url_userinfo(&p.base_url),
        cli_config::mask(&p.api_key)
    )
}

/// Manage named main endpoint profiles from the CLI.
fn run_provider_config(cmd: ProviderConfigCmd) -> Result<()> {
    let mut cfg = cli_config::load();
    match cmd {
        ProviderConfigCmd::Add {
            name,
            base_url,
            api_key,
            model,
            context_window,
            activate,
        } => {
            if cfg.provider(&name).is_some() {
                anyhow::bail!("provider profile already exists: {name}");
            }
            let mut profile =
                cli_config::ProviderProfile::normalized(&name, &base_url, &api_key, &model)?;
            profile.model_context_window = context_window.filter(|n| *n > 0);
            cfg.upsert_provider(profile.clone())?;
            if activate {
                cfg.activate_provider(&profile.name)?;
            }
            cli_config::save(&cfg)?;
            println!(
                "{} provider '{}' saved{}",
                crate::ui::theme::ok("✓"),
                profile.name,
                if activate { " and active" } else { "" }
            );
        }
        ProviderConfigCmd::Edit {
            name,
            base_url,
            api_key,
            model,
            context_window,
        } => {
            let canonical = cfg
                .provider(&name)
                .map(|p| p.name.clone())
                .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {name}"))?;
            let active = cfg
                .active_provider
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(&canonical));
            let mut profile =
                cli_config::ProviderProfile::normalized(&canonical, &base_url, &api_key, &model)?;
            profile.model_context_window = context_window.filter(|n| *n > 0);
            cfg.upsert_provider(profile.clone())?;
            if active {
                cfg.activate_provider(&canonical)?;
            }
            cli_config::save(&cfg)?;
            println!(
                "{} provider '{}' updated",
                crate::ui::theme::ok("✓"),
                canonical
            );
        }
        ProviderConfigCmd::Rename { name, new_name } => {
            cfg.rename_provider(&name, &new_name)?;
            cli_config::save(&cfg)?;
            println!(
                "{} provider '{}' renamed to '{}'",
                crate::ui::theme::ok("✓"),
                name,
                new_name
            );
        }
        ProviderConfigCmd::Use { name } => {
            cfg.activate_provider(&name)?;
            let active = cfg.active_provider.clone().unwrap_or(name);
            cli_config::save(&cfg)?;
            println!("{} provider '{}' active", crate::ui::theme::ok("✓"), active);
            print_provider_env_override_note();
        }
        ProviderConfigCmd::List => {
            let Some(list) = cfg.providers.as_ref() else {
                println!("no provider profiles — add one with `aizen config provider add`");
                return Ok(());
            };
            for p in list {
                println!("{}", provider_row(&cfg, p));
            }
        }
        ProviderConfigCmd::Remove {
            name,
            replace_with,
            force,
        } => {
            let refs = cfg.provider_references(&name);
            if !refs.is_empty() {
                if let Some(replacement) = replace_with.as_deref() {
                    if cfg.provider(replacement).is_none() {
                        anyhow::bail!("unknown replacement provider profile: {replacement}");
                    }
                    cfg.replace_provider_references(&name, replacement)?;
                } else if force {
                    cfg.clear_provider_references(&name);
                } else {
                    anyhow::bail!(
                        "provider '{}' is used by {} — pass --replace-with <provider> or --force to clear those assignments",
                        name,
                        refs.join(", ")
                    );
                }
            }
            let was_active = cfg
                .active_provider
                .as_deref()
                .is_some_and(|active| active.eq_ignore_ascii_case(&name));
            let removed = cfg.remove_provider(&name)?;
            cli_config::save(&cfg)?;
            println!(
                "{} provider '{}' removed{}",
                crate::ui::theme::ok("✓"),
                removed.name,
                if was_active {
                    " (current endpoint kept)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

fn print_provider_env_override_note() {
    let vars = [
        ("AIZEN_BASE_URL", "endpoint"),
        ("AIZEN_API_KEY", "API key"),
        ("AIZEN_MODEL", "model"),
    ];
    let overridden: Vec<&str> = vars
        .iter()
        .filter_map(|(var, label)| {
            std::env::var(var)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|_| *label)
        })
        .collect();
    if !overridden.is_empty() {
        println!(
            "  note: environment variables override the saved provider's {}",
            overridden.join(", ")
        );
    }
}

/// Render the saved config as a grouped, aligned "Studio" panel: a gold title rule with the file
/// path, then sections (Endpoint / Session / Cost / Display) of `key   value` rows where the value's
/// colour carries meaning (gold = a chosen value, green = on/ok, faint = off/unset). Shown at the end
/// of the wizard, after `config set`, and on `aizen config show`.
///
/// Goes through `tui::emit_line`, NOT `println!`. The old claim that a plain print was safe here
/// ("it always runs outside the pinned footer") held only while every caller suspended the renderer
/// first — and suspending is not enough: the render thread keeps folding emissions into its block
/// buffer while suspended and repaints from that buffer on resume, so a raw print is either wiped by
/// the repaint or survives as foreign cells inside later frames. That is the `/config`-mid-chat
/// layout corruption. `emit_line` degrades to plain stdout for the one-shot CLI, where `console`
/// still auto-strips colour under `NO_COLOR`/pipes.
fn print_config(cfg: &cli_config::CliConfig) {
    let width = tui::width().clamp(46, 72);
    let path = cli_config::config_path().display().to_string();

    // ── header: "config" on the left, the file path faint on the right, then a gold rule ──
    let title = "config";
    let used = console::measure_text_width(title) + console::measure_text_width(&path);
    let gap = width.saturating_sub(used + 2).max(1);
    tui::emit_line(&format!(
        "\n  {}{}{}",
        theme::accent(title).bold(),
        " ".repeat(gap),
        theme::faint(&path)
    ));
    tui::emit_line(&format!("  {}", theme::accent_dim("─".repeat(width))));

    // row/section helpers — keys aligned in a fixed column, values free-form (already styled).
    let section = |name: &str| {
        tui::emit_line(&format!(
            "\n  {} {}",
            theme::accent("◆"),
            theme::accent(name).bold()
        ))
    };
    let row = |key: &str, val: String| {
        tui::emit_line(&format!("    {}  {val}", theme::muted(format!("{key:<8}"))))
    };
    let on = |b: bool| {
        if b {
            theme::ok("● on").to_string()
        } else {
            theme::faint("○ off").to_string()
        }
    };
    let unset = || theme::faint("— not set").italic().to_string();
    let tok = |n: usize| {
        if n >= 1000 {
            format!("{}K", n / 1000)
        } else {
            n.to_string()
        }
    };
    // A base URL shouldn't carry credentials, but if one embeds `user:pass@`, `config show` must
    // not print it in the clear — redact the userinfo before display (host/path stay visible).
    let redact_url = |u: &str| -> String { redact_url_userinfo(u) };

    // ── Endpoint ──
    section("Endpoint");
    row(
        "url",
        cfg.base_url
            .clone()
            .map(|v| theme::link(redact_url(&v)).to_string())
            .unwrap_or_else(unset),
    );
    row(
        "key",
        match cfg.api_key.as_deref() {
            Some(k) => format!("{}  {}", cli_config::mask(k), theme::ok("✓")),
            None => format!("{}  {}", unset(), theme::warn("required")),
        },
    );
    row(
        "provider",
        cfg.active_provider
            .as_deref()
            .map(|name| theme::accent(name).to_string())
            .unwrap_or_else(|| theme::faint("direct endpoint").to_string()),
    );
    row(
        "profiles",
        cfg.providers
            .as_ref()
            .map(|list| {
                let names = list
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} · {}", list.len(), theme::faint(names))
            })
            .unwrap_or_else(|| "0 · save the current connection as a provider".to_string()),
    );
    match cfg.model.as_deref() {
        Some(m) => {
            row("model", theme::accent(m).to_string());
            let (w, was_cfg) = effective_ctx_window(m, cfg.model_context_window);
            let note = if was_cfg {
                "from provider"
            } else {
                "estimated by name"
            };
            row(
                "context",
                format!("{} tok  {}", tok(w), theme::faint(format!("· {note}"))),
            );
        }
        None => row(
            "model",
            format!("{}  {}", unset(), theme::faint("· run /model")),
        ),
    }

    // ── Sub-agents & roles ──
    // Printed even when nothing is configured: the whole point of this section is that `config set
    // --subagent-model …` used to write into a file NOTHING displayed, so there was no way to confirm
    // a setting had landed short of opening the JSON.
    print_roles_section(cfg);

    // ── Session ──
    section("Session");
    row(
        "compact",
        match cfg.compact_threshold_pct.unwrap_or(80) {
            0 => format!(
                "{}  {}",
                theme::faint("○ off"),
                theme::faint("· no auto-compaction")
            ),
            t => format!("at {} of context", theme::accent(format!("{t}%"))),
        },
    );
    row(
        "skills",
        format!("auto-learn {}", on(cfg.auto_skill_learn.unwrap_or(true))),
    );
    row(
        "persona",
        format!(
            "{}  {} evolve {}",
            cfg.persona
                .clone()
                .map(|p| theme::accent(p).to_string())
                .unwrap_or_else(|| theme::faint("default voice").to_string()),
            theme::faint("·"),
            on(cfg.persona_evolve.unwrap_or(true))
        ),
    );
    row(
        "timeline",
        match cfg.timemachine_keep.unwrap_or(50) {
            0 => format!(
                "{}  {}",
                theme::accent("unlimited"),
                theme::faint("· keep every checkpoint")
            ),
            k => format!(
                "keep {}  {}",
                theme::accent(k.to_string()),
                theme::faint("· auto-prune oldest")
            ),
        },
    );
    row(
        "snapshot budget",
        format!(
            "{} files · {} total · {} each",
            cfg.timemachine_max_files.unwrap_or(100_000),
            fmt_bytes(cfg.timemachine_max_bytes.unwrap_or(2 * 1024 * 1024 * 1024)),
            fmt_bytes(cfg.timemachine_max_file_bytes.unwrap_or(512 * 1024 * 1024))
        ),
    );

    // ── Memory ──
    // Reports the tier that will ACTUALLY run, not the config flag: `settings()` folds in the cargo
    // feature, `AIZEN_MEM_DENSE`, and whether a model is installed. Printing the flag alone would
    // claim dense recall on a build that has no semantic backend.
    section("Memory");
    row("auto-learn", on(cfg.memory_auto_learn.unwrap_or(true)));
    row(
        "recall",
        if memory::settings().enable_dense {
            format!("{}  {}", theme::accent("lexical + dense"), theme::ok("✓"))
        } else if cfg!(feature = "dense") {
            format!(
                "{}  {}",
                theme::accent("lexical"),
                theme::faint("· no embedding model installed")
            )
        } else {
            format!(
                "{}  {}",
                theme::accent("lexical"),
                theme::faint("· this build has no semantic backend")
            )
        },
    );
    row(
        "embed model",
        match cfg.embed_model.as_deref() {
            Some(m) => theme::accent(m).to_string(),
            None => format!(
                "{}  {}",
                theme::faint("auto"),
                theme::faint(format!(
                    "· {}",
                    memory::embed::discover_local_model()
                        .map(|c| c.name)
                        .unwrap_or_else(|| "none found".into())
                ))
            ),
        },
    );

    // ── Web search ──
    // Both keys are listed because `/config` now edits both, and because the "needs a key" warning is
    // only true when NEITHER is present: Jina alone is a working (if secondary) search backend, so
    // warning next to a set Jina key would be wrong.
    section("Web search");
    let tavily_key = cfg.reach.as_ref().and_then(|r| r.resolved_tavily_key());
    let jina_key = cfg.reach.as_ref().and_then(|r| r.resolved_jina_key());
    row(
        "tavily key",
        match &tavily_key {
            Some(k) => format!("{}  {}", cli_config::mask(k), theme::ok("✓")),
            None if jina_key.is_some() => format!("{}  {}", unset(), theme::faint("· using jina")),
            None => format!(
                "{}  {}",
                unset(),
                theme::warn("web_search needs a key · run config")
            ),
        },
    );
    row(
        "jina key",
        match &jina_key {
            Some(k) => format!("{}  {}", cli_config::mask(k), theme::ok("✓")),
            None => format!("{}  {}", unset(), theme::faint("· optional fallback")),
        },
    );

    // ── Cost ──
    section("Cost");
    row(
        "pricing",
        match (cfg.price_in, cfg.price_out) {
            (Some(pin), Some(pout)) => format!(
                "{} / {} {}",
                theme::ok(format!("${pin}")),
                theme::ok(format!("${pout}")),
                theme::faint("per 1M tok · in/out")
            ),
            _ => format!("{}  {}", unset(), theme::faint("· /cost shows tokens only")),
        },
    );

    // ── Display ──
    section("Display");
    row(
        "icons",
        theme::accent(cfg.icons.as_deref().unwrap_or("nerd")).to_string(),
    );
    row(
        "visuals",
        theme::accent(cfg.response_visuals().to_string()).to_string(),
    );
    println!();
}

/// A base URL shouldn't carry credentials, but if one embeds `user:pass@`, any display path must not
/// print it in the clear — redact the userinfo before display (host/path stay visible).
pub(crate) fn redact_url_userinfo(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
            let _ = parsed.set_username("•••");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        _ => u.to_string(),
    }
}

/// Render one role's `api_key_ref` for display, NEVER its value.
///
/// Two shapes, deliberately different: `env:VAR` prints the VARIABLE NAME plus whether that variable
/// is actually set right now, because the failure this catches is a forgotten `export` — you need to
/// see `env:OPENAI_KEY ✗ unset` to know why sub-agents are 401-ing. A literal key prints through
/// [`cli_config::mask`], same as the main key.
fn role_key_display(api_key_ref: Option<&str>) -> Option<String> {
    let raw = api_key_ref.map(str::trim).filter(|s| !s.is_empty())?;
    Some(match raw.strip_prefix("env:").map(str::trim) {
        Some(var) if !var.is_empty() => {
            if std::env::var(var)
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
            {
                format!("env:{var} {}", theme::ok("✓"))
            } else {
                format!("env:{var} {}", theme::warn("✗ unset"))
            }
        }
        // `env:` with nothing after it, or a literal key.
        _ => cli_config::mask(raw),
    })
}

/// One `Sub-agents & roles` row: `model · base_url · key`, with each absent field simply omitted so
/// a role that only pins a model reads as one short line instead of three "not set" clauses.
fn role_row_value(rc: Option<&cli_config::RoleModelConfig>) -> String {
    let Some(rc) = rc else {
        return format!(
            "{}  {}",
            theme::faint("— not set").italic(),
            theme::faint("· uses the main endpoint")
        );
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(provider) = rc.provider.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(theme::accent(format!("provider:{provider}")).to_string());
    }
    if let Some(m) = rc.model.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(theme::accent(m).to_string());
    }
    if let Some(u) = rc.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(theme::link(redact_url_userinfo(u)).to_string());
    }
    if let Some(k) = role_key_display(rc.api_key_ref.as_deref()) {
        parts.push(k);
    }
    if parts.is_empty() {
        return format!(
            "{}  {}",
            theme::faint("— not set").italic(),
            theme::faint("· uses the main endpoint")
        );
    }
    parts.join(&format!(" {} ", theme::faint("·")))
}

/// The `Sub-agents & roles` panel: per-role endpoints plus the model→endpoint registry.
///
/// `oracle` carries an extra note because configuring it is not just "pick a model" — `self_review()`
/// falls back to `role_configured("oracle")`, so setting this role is what TURNS SELF-REVIEW ON. A
/// reader who doesn't know that would set an oracle model and be surprised by the extra review turn.
fn print_roles_section(cfg: &cli_config::CliConfig) {
    println!(
        "\n  {} {}",
        theme::accent("◆"),
        theme::accent("Sub-agents & roles").bold()
    );
    let row =
        |key: &str, val: String| println!("    {}  {val}", theme::muted(format!("{key:<10}")));
    let roles = cfg.roles.as_ref();
    row(
        "subagent",
        role_row_value(roles.and_then(|r| r.subagent_default.as_ref())),
    );
    row(
        "summarizer",
        role_row_value(roles.and_then(|r| r.summarizer.as_ref())),
    );
    let oracle = roles.and_then(|r| r.oracle.as_ref());
    row(
        "oracle",
        format!(
            "{}  {}",
            role_row_value(oracle),
            if cfg.self_review.unwrap_or(oracle.is_some()) {
                theme::ok("· self-review ● on").to_string()
            } else {
                theme::faint("· self-review ○ off").to_string()
            }
        ),
    );
    row(
        "apply",
        role_row_value(roles.and_then(|r| r.apply.as_ref())),
    );
    match cfg.agent_routes.as_deref().filter(|l| !l.is_empty()) {
        Some(routes) => row(
            "specialists",
            theme::accent(format!("{} provider assignment(s)", routes.len())).to_string(),
        ),
        None => row(
            "specialists",
            format!(
                "{}  {}",
                theme::faint("— inherit").italic(),
                theme::faint("· use subagent default")
            ),
        ),
    }
    match cfg.model_endpoints.as_deref().filter(|l| !l.is_empty()) {
        Some(list) => {
            let names: Vec<&str> = list.iter().map(|e| e.model.as_str()).collect();
            row(
                "advanced",
                format!(
                    "{}  {}",
                    theme::warn(format!("{} model endpoint override(s)", list.len())),
                    theme::faint(format!("· {}", names.join(", ")))
                ),
            );
        }
        None => row(
            "advanced",
            format!(
                "{}  {}",
                theme::faint("— none").italic(),
                theme::faint("· provider routing is unmasked")
            ),
        ),
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1}GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1}MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1}KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// Given the fetched models + the currently-saved model, the index Select should default to.
pub(crate) fn model_default_index(models: &[String], current: Option<&str>) -> usize {
    current
        .and_then(|m| models.iter().position(|x| x == m))
        .unwrap_or(0)
}

const CUSTOM_MODEL_ITEM: &str = "‹ type a custom id ›";

/// `aizen config` / `/config` / menu → Setup. A fresh install (no endpoint yet) gets the guided
/// linear setup once so nothing required is missed; an already-configured user gets the HUB menu —
/// jump to the one section you want, edit just it, it saves on the spot, you're back at the menu. No
/// more Enter-through-every-prompt to change a single field.
pub(crate) async fn config_wizard() -> Result<()> {
    let cfg = cli_config::load();
    let fresh = cfg.base_url.is_none() || cfg.api_key.is_none() || cfg.model.is_none();
    if fresh {
        let mut cfg = cfg;
        return config_setup_full(&mut cfg).await;
    }
    config_menu(cfg).await
}

/// A yes/no toggle with the shared gold theme (used by the section editors below).
fn yn(theme: &ColorfulTheme, prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::with_theme(theme)
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

// ── setup: validated connection (provider → base URL → key → model) ──────────

/// A known endpoint the user can pick instead of typing a URL. `base` is stored verbatim, so every
/// entry here must already carry whatever version suffix the provider needs — that is the whole point
/// of a preset: the `/v1` that people forget is baked in and can't be forgotten.
struct ProviderPreset {
    label: &'static str,
    base: &'static str,
    /// Where to get a key, shown right before we ask for one.
    keys_url: &'static str,
    /// A model id that exists there, used only as the manual-entry default if the list fetch fails.
    sample_model: &'static str,
}

/// Presets offered by the provider picker, in menu order.
///
/// Anthropic is here as an OpenAI-COMPATIBLE entry, which is worth being precise about: aizen speaks
/// `POST {base}/chat/completions` with a Bearer token, and Anthropic serves exactly that shape at
/// `https://api.anthropic.com/v1/` (their documented OpenAI-SDK compatibility surface, where
/// `authorization` is fully supported). So this preset needs no new wire protocol. The one wrinkle is
/// `GET /v1/models`, which is the NATIVE endpoint and wants `x-api-key` + `anthropic-version` —
/// handled in `client::with_provider_auth`, not here.
///
/// OpenCode's zen gateway is the other entry with a wrinkle: its free tier authenticates with the
/// literal shared token `public` (the `keys_url` row says so, mirroring the Ollama no-key pattern)
/// and expects the `x-opencode-client` header — also attached in `client::with_provider_auth`. Its
/// free models are the `-free`-suffixed ids in the live `/models` list; `sample_model` is only the
/// manual-entry default if that fetch fails.
const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        label: "OpenAI",
        base: "https://api.openai.com/v1",
        keys_url: "https://platform.openai.com/api-keys",
        sample_model: "gpt-4o",
    },
    ProviderPreset {
        label: "ChatGPT Codex (experimental)",
        base: crate::llm::oauth_codex::CODEX_BASE_URL,
        keys_url: "run: aizen auth login codex  (experimental ChatGPT OAuth — see docs RISK)",
        sample_model: "gpt-5.4-mini",
    },
    ProviderPreset {
        label: "Anthropic (Claude)",
        base: "https://api.anthropic.com/v1",
        keys_url: "https://console.anthropic.com/settings/keys",
        sample_model: "claude-opus-5",
    },
    ProviderPreset {
        label: "OpenRouter",
        base: "https://openrouter.ai/api/v1",
        keys_url: "https://openrouter.ai/keys",
        sample_model: "anthropic/claude-opus-5",
    },
    ProviderPreset {
        label: "Groq",
        base: "https://api.groq.com/openai/v1",
        keys_url: "https://console.groq.com/keys",
        sample_model: "llama-3.3-70b-versatile",
    },
    ProviderPreset {
        label: "DeepSeek",
        base: "https://api.deepseek.com/v1",
        keys_url: "https://platform.deepseek.com/api_keys",
        sample_model: "deepseek-chat",
    },
    ProviderPreset {
        label: "OpenCode (free)",
        base: "https://opencode.ai/zen/v1",
        keys_url: "no key needed — free tier: enter the shared token `public`",
        sample_model: "deepseek-v4-flash-free",
    },
    ProviderPreset {
        label: "Ollama (local)",
        base: "http://localhost:11434/v1",
        keys_url: "no key needed — enter anything (e.g. `ollama`)",
        sample_model: "llama3.2",
    },
];

/// Await `fut` while a spinner animates on the line, then clear it. The verdict is the caller's to
/// print — this only owns the "something is happening" gap.
///
/// Safe to draw here because every entry point into config SUSPENDS the retained TUI
/// (`tui::slash_takes_stdin` lists `config`/`setup`), so stdout is ours. `Spinner` is itself a no-op
/// off a TTY, so piped runs stay clean.
async fn spin_while<T>(label: &str, fut: impl std::future::Future<Output = T>) -> T {
    let sp = crate::ui::spinner::Spinner::start(label);
    let out = fut.await;
    drop(sp); // clears the line, leaves the cursor at column 0
    out
}

// The three status lines below go through `tui::emit_line` for the reason spelled out on
// `print_config`: a raw print during a suspended menu is either wiped by resume's repaint or
// survives as foreign cells in later frames. Outside the REPL `emit_line` is a plain stdout write,
// so the one-shot `aizen config` path is unchanged.

/// `  ✓ <msg>` in the ok colour.
fn line_ok(msg: &str) {
    tui::emit_line(&format!(
        "  {} {}",
        crate::ui::theme::ok("✓"),
        style(msg).dim()
    ));
}

/// `  ✗ <msg>` in red — a failure the user has to act on.
fn line_bad(msg: &str) {
    tui::emit_line(&format!("  {} {}", style("✗").red(), style(msg).red()));
}

/// `  ! <msg>` in the warn colour — something to know, but not a stop.
fn line_warn(msg: &str) {
    tui::emit_line(&format!(
        "  {} {}",
        style("!").color256(crate::ui::theme::WARN),
        style(msg).color256(crate::ui::theme::WARN)
    ));
}

/// Ask for a base URL until one actually answers as a models endpoint, then return it with the model
/// list the check already fetched.
///
/// Two deliberate choices:
///
/// * **A missing `/v1` is diagnosed, not just reported.** It is the single most common setup mistake,
///   and the failure it produces (404 on `{base}/models`) is indistinguishable from a typo unless we
///   say so. When the URL has no version segment we offer the `/v1` form as the next default, so
///   fixing it is one Enter.
/// * **The check runs BEFORE asking for a key** and passes `None`. An endpoint that answers 401
///   without credentials has already proven it is reachable and speaks the protocol, which is exactly
///   what this step needs to establish; asking for a key first would blame the key for a bad URL.
///
/// `current` pre-fills the prompt (Enter keeps it). Returns `None` if the user gives up (Esc/Ctrl-C
/// propagate as errors; an empty entry with `allow_skip` returns `None`).
async fn prompt_validated_base_url(
    theme: &ColorfulTheme,
    http: &reqwest::Client,
    current: Option<&str>,
    allow_skip: bool,
) -> Result<Option<(String, Vec<client::ModelInfo>)>> {
    let mut suggestion: Option<String> = current.map(str::to_string);
    loop {
        let mut input = Input::<String>::with_theme(theme)
            .with_prompt("Base URL (must include the version path, e.g. https://api.openai.com/v1)")
            .allow_empty(allow_skip);
        if let Some(s) = suggestion.clone() {
            input = input.default(s);
        }
        let raw = input.interact_text()?;
        let base = raw.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            if allow_skip {
                return Ok(None);
            }
            line_bad("a base URL is required");
            continue;
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            line_bad("must start with http:// or https://");
            suggestion = Some(format!("https://{base}"));
            continue;
        }

        let check = spin_while(
            &format!("checking {base}"),
            client::check_endpoint(http, &base, None),
        )
        .await;
        match check {
            client::EndpointCheck::Ok(infos) => {
                line_ok(&format!("reachable — {} models", infos.len()));
                return Ok(Some((base, infos)));
            }
            // Reachable + speaks the protocol; it just wants credentials, which is the next step.
            client::EndpointCheck::Auth(_) => {
                line_ok("reachable (needs a key — next step)");
                return Ok(Some((base, Vec::new())));
            }
            client::EndpointCheck::NotFound(detail) => {
                line_bad(&format!("no model list at {base}/models"));
                if !detail.is_empty() {
                    tui::emit_line(&format!("    {}", style(&detail).dim()));
                }
                match missing_version_suffix(&base) {
                    Some(fixed) => {
                        line_warn(&format!("most endpoints need a version path — try {fixed}"));
                        suggestion = Some(fixed);
                    }
                    None => suggestion = Some(base),
                }
            }
            client::EndpointCheck::Unreachable(detail) => {
                line_bad(&format!("could not reach it: {detail}"));
                suggestion = Some(base);
            }
            client::EndpointCheck::Http(code, detail) => {
                line_bad(&format!("HTTP {code}"));
                if !detail.is_empty() {
                    tui::emit_line(&format!("    {}", style(&detail).dim()));
                }
                suggestion = Some(base);
            }
        }
        if allow_skip && !yn(theme, "Try a different URL?", true)? {
            return Ok(None);
        }
    }
}

/// `Some(base + "/v1")` when `base` has no version-looking final segment, else `None`.
///
/// "Already versioned" means the last segment is `v` + at least one digit, optionally followed by
/// more alphanumerics — so `v1`, `v2`, and `v1beta` all count. The trailing-suffix allowance is not
/// cosmetic: several providers ship `/v1beta`, and telling that user to try `/v1beta/v1` would send
/// them somewhere that definitely doesn't exist.
///
/// A segment like `/api` or `/openai` is a path, not a version, so it still gets the hint — that's
/// the case that otherwise leaves someone stuck on a 404 with nothing to try.
fn missing_version_suffix(base: &str) -> Option<String> {
    let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let last = after_scheme
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let versioned = last
        .strip_prefix('v')
        .or_else(|| last.strip_prefix('V'))
        .is_some_and(|rest| {
            let mut chars = rest.chars();
            // At least one digit right after the `v`, then anything alphanumeric (v1, v2, v1beta).
            chars.next().is_some_and(|c| c.is_ascii_digit())
                && chars.all(|c| c.is_ascii_alphanumeric())
        });
    if versioned {
        None
    } else {
        Some(format!("{}/v1", base.trim_end_matches('/')))
    }
}

/// Ask for an API key until the endpoint accepts it, returning it with the model list.
///
/// The key is entered in the CLEAR, not through `Password`. Pasting a 100-char secret into an
/// invisible field gives you no way to see a truncated paste or a stray newline — and the usual
/// justification (shoulder-surfing) doesn't hold when the value is one keystroke from being saved to
/// a plaintext config file the user can `cat`. Only the ECHO changes: nothing new is logged, and the
/// stored value is still masked everywhere it's displayed later.
///
/// `keys_url` (when known) is printed first, so someone without a key isn't sent hunting.
async fn prompt_validated_api_key(
    theme: &ColorfulTheme,
    http: &reqwest::Client,
    base: &str,
    current: Option<&str>,
    keys_url: Option<&str>,
) -> Result<Option<(String, Vec<client::ModelInfo>)>> {
    if let Some(url) = keys_url {
        tui::emit_line(&format!("  {}", style(format!("get a key: {url}")).dim()));
    }
    loop {
        let prompt = match current {
            Some(k) => format!("API key (current {} — Enter keeps it)", cli_config::mask(k)),
            None => "API key (visible as you type, so you can check the paste)".to_string(),
        };
        let entered = Input::<String>::with_theme(theme)
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?;
        let entered = entered.trim().to_string();
        let candidate = if entered.is_empty() {
            match current {
                Some(k) => k.to_string(),
                None => {
                    line_bad("a key is required");
                    continue;
                }
            }
        } else {
            entered
        };

        let check = spin_while(
            "verifying the key",
            client::check_endpoint(http, base, Some(&candidate)),
        )
        .await;
        match check {
            client::EndpointCheck::Ok(infos) => {
                line_ok(&format!("key accepted — {} models available", infos.len()));
                return Ok(Some((candidate, infos)));
            }
            client::EndpointCheck::Auth(detail) => {
                line_bad("the endpoint rejected that key");
                if !detail.is_empty() {
                    tui::emit_line(&format!("    {}", style(&detail).dim()));
                }
            }
            // Not the key's fault — don't make them re-paste a key that may be fine.
            other => {
                let what = match &other {
                    client::EndpointCheck::NotFound(d) => {
                        format!("no model list at this path ({d})")
                    }
                    client::EndpointCheck::Unreachable(d) => format!("could not reach it ({d})"),
                    client::EndpointCheck::Http(c, d) => format!("HTTP {c} ({d})"),
                    client::EndpointCheck::Ok(_) | client::EndpointCheck::Auth(_) => unreachable!(),
                };
                line_warn(&format!("could not verify the key — {what}"));
                if yn(theme, "Keep this key anyway?", true)? {
                    return Ok(Some((candidate, Vec::new())));
                }
            }
        }
        if !yn(theme, "Enter a different key?", true)? {
            return Ok(None);
        }
    }
}

/// The provider step: pick a preset (URL pre-filled, version suffix already correct) or type a custom
/// endpoint. Returns the chosen preset, or `None` for "custom / I'll type it".
fn prompt_provider(
    theme: &ColorfulTheme,
    current_base: Option<&str>,
) -> Result<Option<&'static ProviderPreset>> {
    let mut items: Vec<String> = PROVIDER_PRESETS
        .iter()
        .map(|p| format!("{:<20} {}", p.label, p.base))
        .collect();
    // An endpoint that matches no preset belongs to this row, so show it ON the row. Otherwise a
    // gateway/proxy/self-hosted user sees every preset's URL printed but not their own, which reads
    // as "my endpoint isn't in this list" rather than "it's the row I'm already standing on".
    let custom_current = current_base
        .map(|b| b.trim_end_matches('/'))
        .filter(|b| {
            !PROVIDER_PRESETS
                .iter()
                .any(|p| p.base.trim_end_matches('/') == *b)
        })
        .map(str::to_string);
    items.push(format!(
        "{:<20} {}",
        "Custom gateway",
        custom_current
            .as_deref()
            .unwrap_or("self-hosted / proxy / any OpenAI-compatible — type a URL")
    ));
    // Land on the preset the user is already using, so re-entering the section doesn't silently
    // propose a different provider.
    let default = current_base
        .and_then(|b| {
            let b = b.trim_end_matches('/');
            PROVIDER_PRESETS
                .iter()
                .position(|p| p.base.trim_end_matches('/') == b)
        })
        .unwrap_or(items.len() - 1);
    let pick = Select::with_theme(theme)
        .with_prompt("Provider")
        .items(&items)
        .default(default)
        .interact()?;
    Ok(PROVIDER_PRESETS.get(pick))
}

/// The config HUB: a `Select` of sections, each row showing its current value so the panel reads as a
/// live dashboard. Pick a section → edit just that → it saves immediately → back to the menu. Esc or
/// "Done" exits. Every field here is also scriptable via `aizen config set`, so nothing depends on it.
async fn config_menu(mut cfg: cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    loop {
        // Glanceable current-value hints, one per row.
        let model_h = cfg.model.clone().unwrap_or_else(|| "not set".into());
        let tavily_h = if cfg
            .reach
            .as_ref()
            .and_then(|r| r.resolved_tavily_key())
            .is_some()
        {
            "set"
        } else {
            "none"
        };
        let compact_h = match cfg.compact_threshold_pct.unwrap_or(80) {
            0 => "off".to_string(),
            t => format!("{t}%"),
        };
        let effort_h = if cfg.ultimate.unwrap_or(false) {
            "ultimate".to_string()
        } else if cfg.auto_effort == Some(false) {
            cfg.reasoning_effort
                .clone()
                .unwrap_or_else(|| "fixed".into())
        } else {
            "auto".to_string()
        };
        let approval_h = cfg.persisted_approval_mode().to_string();
        let icons_h = cfg.icons.clone().unwrap_or_else(|| "nerd".into());
        let visuals_h = cfg.response_visuals().to_string();

        let items = vec![
            format!(
                "Providers & connection · {} · {}",
                cfg.active_provider
                    .as_deref()
                    .unwrap_or("unsaved connection"),
                model_h
            ),
            format!(
                "Main model & context · {}",
                cfg.model.as_deref().unwrap_or("not set")
            ),
            format!("Sub-agents      · {}", subagent_hint(&cfg)),
            format!("Web search      · tavily {tavily_h}"),
            format!("Memory          · {}", memory_hint(&cfg)),
            format!("Session         · compact {compact_h}"),
            format!("Reasoning       · {effort_h}"),
            format!("Approval        · {approval_h}"),
            format!("Display         · icons {icons_h} · visuals {visuals_h}"),
            "Show full config".to_string(),
            "Done".to_string(),
        ];
        let pick = match Select::with_theme(&theme)
            .with_prompt("Config — pick a section (Esc when done)")
            .items(&items)
            .default(0)
            .interact_opt()?
        {
            Some(i) => i,
            None => break,
        };
        // Sections 0..=8 edit + save; 9 shows the panel; 10 (or Esc) exits.
        let edited = match pick {
            0 => config_edit_providers(&mut cfg).await,
            1 => config_edit_model(&mut cfg).await,
            2 => config_edit_subagents(&mut cfg).await,
            3 => config_edit_websearch(&mut cfg).await,
            4 => config_edit_memory(&mut cfg),
            5 => config_edit_session(&mut cfg),
            6 => config_edit_reasoning(&mut cfg),
            7 => config_edit_approval(&mut cfg),
            8 => config_edit_display(&mut cfg),
            9 => {
                print_config(&cfg);
                continue;
            }
            _ => break,
        };
        match edited {
            Ok(()) => match cli_config::save(&cfg) {
                Ok(_) => tui::emit_line(&format!(
                    "  {} {}",
                    crate::ui::theme::ok("✓"),
                    style("saved").color256(splash::ACCENT)
                )),
                Err(e) => tui::note_line(&format!("  {} {e}", style("save:").red())),
            },
            Err(e) => tui::note_line(&format!("  {} {e}", style("config:").red())),
        }
    }
    Ok(())
}

/// Section editor: provider → base URL → API key, each step verified against the live endpoint
/// before it is accepted. Nothing is written to `cfg` until a step actually passes, so a failed
/// attempt leaves the previous working connection intact.
#[allow(dead_code)]
async fn config_edit_connection(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let http = http_client()?;
    let previous_base = cfg.base_url.clone();

    let preset = prompt_provider(&theme, cfg.base_url.as_deref())?;
    // A preset's URL is already correct, so it only needs the reachability check — not the
    // type-it-again loop. A custom endpoint goes through the full prompt.
    let (base, mut infos) = match preset {
        Some(p) => {
            let check = spin_while(
                &format!("checking {}", p.base),
                client::check_endpoint(&http, p.base, None),
            )
            .await;
            match check {
                client::EndpointCheck::Ok(infos) => {
                    line_ok(&format!("reachable — {} models", infos.len()));
                    (p.base.to_string(), infos)
                }
                client::EndpointCheck::Auth(_) => {
                    line_ok("reachable (needs a key — next step)");
                    (p.base.to_string(), Vec::new())
                }
                // Even a preset can be unreachable (Ollama not running, network down, provider
                // outage). Say so and let them keep it or type something else, rather than pretending.
                other => {
                    let what = match &other {
                        client::EndpointCheck::NotFound(d) => format!("no model list there ({d})"),
                        client::EndpointCheck::Unreachable(d) => {
                            format!("could not reach it ({d})")
                        }
                        client::EndpointCheck::Http(c, d) => format!("HTTP {c} ({d})"),
                        _ => unreachable!(),
                    };
                    line_warn(&format!("{} — {what}", p.label));
                    if yn(&theme, "Use this URL anyway?", true)? {
                        (p.base.to_string(), Vec::new())
                    } else {
                        match prompt_validated_base_url(&theme, &http, Some(p.base), true).await? {
                            Some(v) => v,
                            None => return Ok(()),
                        }
                    }
                }
            }
        }
        None => {
            match prompt_validated_base_url(&theme, &http, cfg.base_url.as_deref(), true).await? {
                Some(v) => v,
                None => return Ok(()),
            }
        }
    };
    let same_endpoint = previous_base
        .as_deref()
        .map(|url| url.trim().trim_end_matches('/'))
        == Some(base.as_str());

    let keys_url = preset.map(|p| p.keys_url);
    let current_key = same_endpoint.then_some(cfg.api_key.as_deref()).flatten();
    let (key, fetched) =
        match prompt_validated_api_key(&theme, &http, &base, current_key, keys_url).await? {
            Some(v) => v,
            None => return Ok(()),
        };
    if !fetched.is_empty() {
        infos = fetched;
    }

    let mut draft = cfg.clone();
    draft.base_url = Some(base);
    draft.api_key = Some(key);
    if !same_endpoint {
        draft.model = None;
        draft.model_context_window = None;
        draft.active_provider = None;
    }

    // The key check already fetched the list — offering it here saves a redundant round-trip and
    // means a fresh connection lands on a working model instead of whatever was configured before.
    if !infos.is_empty() && yn(&theme, "Pick a model now?", true)? {
        pick_model_from(&theme, &mut draft, &infos, preset.map(|p| p.sample_model))?;
    } else if draft.model.is_none() {
        let mut input = Input::<String>::with_theme(&theme)
            .with_prompt("Model id")
            .allow_empty(true);
        if let Some(sample) = preset.map(|p| p.sample_model.to_string()) {
            input = input.default(sample);
        }
        let model = input.interact_text()?;
        if model.trim().is_empty() {
            line_warn("connection unchanged — a model is required for the new URL");
            return Ok(());
        }
        draft.model = Some(model.trim().to_string());
    }
    draft.sync_active_provider();
    *cfg = draft;
    Ok(())
}

/// Manage provider profiles from the config hub. Adding/editing reuses the validated Connection flow;
/// the complete resulting tuple is then stored under the chosen name.
pub(crate) async fn config_edit_providers(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let http = http_client()?;
    loop {
        let list = cfg.providers.clone().unwrap_or_default();
        let mut items: Vec<String> = list.iter().map(|p| provider_row(cfg, p)).collect();
        items.push("＋ Add provider".to_string());
        items.push("Back".to_string());
        let pick = match Select::with_theme(&theme)
            .with_prompt("Providers (Esc when done)")
            .items(&items)
            .default(items.len().saturating_sub(2))
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick == items.len() - 1 {
            return Ok(());
        }
        if pick == list.len() {
            let name: String = Input::with_theme(&theme)
                .with_prompt("Provider name (no spaces)")
                .allow_empty(true)
                .interact_text()?;
            if name.trim().is_empty() || cfg.provider(&name).is_some() {
                line_bad("provider name is empty or already exists");
                continue;
            }
            let base = match prompt_validated_base_url(&theme, &http, None, true).await? {
                Some((url, _)) => url,
                None => continue,
            };
            let (key, mut infos) =
                match prompt_validated_api_key(&theme, &http, &base, None, None).await? {
                    Some(v) => v,
                    None => continue,
                };
            if infos.is_empty() {
                if let Ok(fetched) = client::fetch_models_info(&http, &base, &key).await {
                    infos = fetched;
                }
            }
            let Some(model) = prompt_required_provider_model(&theme, &infos, None)? else {
                continue;
            };
            let mut draft = cli_config::ProviderProfile::normalized(&name, &base, &key, &model)?;
            draft.model_context_window = None;
            cfg.upsert_provider(draft.clone())?;
            if yn(&theme, "Use this provider now?", true)? {
                cfg.activate_provider(&draft.name)?;
            }
            continue;
        }

        let existing = &list[pick];
        let action = Select::with_theme(&theme)
            .with_prompt(format!("{} (Esc cancels)", existing.name))
            .items(&["use now", "edit endpoint + key + model", "rename", "remove"])
            .default(0)
            .interact_opt()?;
        match action {
            Some(0) => cfg.activate_provider(&existing.name)?,
            Some(1) => {
                let old = existing.clone();
                let base = match prompt_validated_base_url(&theme, &http, Some(&old.base_url), true)
                    .await?
                {
                    Some((url, _)) => url,
                    None => continue,
                };
                let same = base.trim_end_matches('/') == old.base_url.trim_end_matches('/');
                let current = same.then_some(old.api_key.as_str());
                let (key, mut infos) =
                    match prompt_validated_api_key(&theme, &http, &base, current, None).await? {
                        Some(v) => v,
                        None => continue,
                    };
                if infos.is_empty() {
                    if let Ok(fetched) = client::fetch_models_info(&http, &base, &key).await {
                        infos = fetched;
                    }
                }
                let Some(model) = prompt_required_provider_model(&theme, &infos, Some(&old.model))?
                else {
                    continue;
                };
                let mut replacement =
                    cli_config::ProviderProfile::normalized(&old.name, &base, &key, &model)?;
                replacement.model_context_window = old.model_context_window;
                let active = cfg
                    .active_provider
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&old.name));
                cfg.upsert_provider(replacement)?;
                if active {
                    cfg.activate_provider(&old.name)?;
                }
            }
            Some(2) => {
                let new_name: String = Input::with_theme(&theme)
                    .with_prompt("New provider name")
                    .default(existing.name.clone())
                    .interact_text()?;
                cfg.rename_provider(&existing.name, &new_name)?;
            }
            Some(3) => {
                let refs = cfg.provider_references(&existing.name);
                if !refs.is_empty() {
                    line_warn(&format!("used by {}", refs.join(", ")));
                    let choices = ["clear assignments and remove", "cancel"];
                    if Select::with_theme(&theme)
                        .with_prompt("This provider is in use")
                        .items(&choices)
                        .default(1)
                        .interact()?
                        == 0
                    {
                        cfg.clear_provider_references(&existing.name);
                    } else {
                        continue;
                    }
                }
                cfg.remove_provider(&existing.name)?;
            }
            _ => {}
        }
    }
}

/// Present `infos` as a picker and store the choice (plus its reported context window). Esc keeps the
/// current model. The last row is a manual-id escape hatch for a model the provider doesn't list.
fn pick_model_from(
    theme: &ColorfulTheme,
    cfg: &mut cli_config::CliConfig,
    infos: &[client::ModelInfo],
    sample_model: Option<&str>,
) -> Result<()> {
    let ids: Vec<String> = infos.iter().map(|m| m.id.clone()).collect();
    let mut items: Vec<String> = infos
        .iter()
        .map(|m| {
            // Free-tier ids get a tag so someone on a free gateway doesn't pick a paid model by
            // accident — the failure would only surface on the first real turn.
            let free = if m.is_free || client::is_free_model_id(&m.id) {
                "  · free"
            } else {
                ""
            };
            match m.context_length {
                Some(n) => format!("{}{free}  ({} ctx)", m.id, n),
                None => format!("{}{free}", m.id),
            }
        })
        .collect();
    items.push(CUSTOM_MODEL_ITEM.to_string());
    let pick = match Select::with_theme(theme)
        .with_prompt("Model (Esc keeps current)")
        .items(&items)
        .default(model_default_index(&ids, cfg.model.as_deref()))
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    if pick < infos.len() {
        cfg.model = Some(infos[pick].id.clone());
        // Provider-reported window when it gave one, else clear it so the HUD uses its heuristic
        // rather than keeping the PREVIOUS model's number, which would be wrong for this one.
        cfg.model_context_window = infos[pick].context_length;
    } else {
        let mut mi = Input::<String>::with_theme(theme).with_prompt("Model id");
        if let Some(s) = cfg
            .model
            .clone()
            .or_else(|| sample_model.map(str::to_string))
        {
            mi = mi.default(s);
        }
        let m = mi.allow_empty(true).interact_text()?;
        if !m.trim().is_empty() {
            cfg.model = Some(m.trim().to_string());
            cfg.model_context_window = None; // custom id → heuristic
        }
    }
    Ok(())
}

/// One-line menu hint for the Sub-agents row: what a dispatched sub-agent runs on today.
fn subagent_hint(cfg: &cli_config::CliConfig) -> String {
    let model = cfg
        .roles
        .as_ref()
        .and_then(|r| r.subagent_default.as_ref())
        .and_then(|s| s.model.clone())
        .unwrap_or_else(|| "main model".to_string());
    let mapped = cfg.model_endpoints.as_deref().map_or(0, <[_]>::len);
    let extra_roles = cfg.roles.as_ref().map_or(0, |r| {
        usize::from(r.summarizer.is_some())
            + usize::from(r.oracle.is_some())
            + usize::from(r.apply.is_some())
    });
    let mut s = model;
    if mapped > 0 {
        s.push_str(&format!(" · {mapped} mapped"));
    }
    if extra_roles > 0 {
        s.push_str(&format!(" · {extra_roles} role(s)"));
    }
    s
}

/// The four routable roles, in menu order: (config key, menu label, what it actually does).
const ROLE_ROWS: [(&str, &str, &str); 4] = [
    (
        "subagent_default",
        "Sub-agent default",
        "the model `task` dispatches run on",
    ),
    (
        "summarizer",
        "Summarizer",
        "compaction + handoff summaries (a cheap-fast model fits)",
    ),
    (
        "oracle",
        "Oracle",
        "the self-review reviewer — SETTING THIS TURNS SELF-REVIEW ON",
    ),
    (
        "apply",
        "Apply",
        "reserved for a fast-apply edit model (config-only today)",
    ),
];

fn role_slot<'a>(
    roles: &'a mut cli_config::RolesConfig,
    role: &str,
) -> &'a mut Option<cli_config::RoleModelConfig> {
    match role {
        "summarizer" => &mut roles.summarizer,
        "oracle" => &mut roles.oracle,
        "apply" => &mut roles.apply,
        _ => &mut roles.subagent_default,
    }
}

fn role_get<'a>(
    cfg: &'a cli_config::CliConfig,
    role: &str,
) -> Option<&'a cli_config::RoleModelConfig> {
    let r = cfg.roles.as_ref()?;
    match role {
        "summarizer" => r.summarizer.as_ref(),
        "oracle" => r.oracle.as_ref(),
        "apply" => r.apply.as_ref(),
        _ => r.subagent_default.as_ref(),
    }
}

/// Section editor: per-role endpoints, the model→endpoint registry, and per-specialist pins.
///
/// This is the surface the config layer never had. `roles.*` and `model_endpoints` have been
/// resolvable since they were added, but the only ways to WRITE them were `config set` flags and
/// hand-editing JSON — and nothing displayed them back, so there was no way to confirm a setting had
/// taken. Everything here edits values that already existed; none of it is new config shape.
async fn config_edit_subagents(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    loop {
        let mut items: Vec<String> = ROLE_ROWS
            .iter()
            .map(|(key, label, _)| {
                let cur = role_get(cfg, key)
                    .map(|rc| {
                        let mut bits: Vec<String> = Vec::new();
                        if let Some(p) = rc.provider.as_deref() {
                            bits.push(format!("provider {p}"));
                        }
                        if let Some(m) = rc.model.as_deref() {
                            bits.push(m.to_string());
                        }
                        if rc.base_url.is_some() {
                            bits.push("own url".into());
                        }
                        if rc.api_key_ref.is_some() {
                            bits.push("own key".into());
                        }
                        if bits.is_empty() {
                            "not set".into()
                        } else {
                            bits.join(" · ")
                        }
                    })
                    .unwrap_or_else(|| "main endpoint".to_string());
                format!("{label:<22}· {cur}")
            })
            .collect();
        items.push(format!(
            "{:<22}· {} advanced entr(ies)",
            "Advanced overrides",
            cfg.model_endpoints.as_deref().map_or(0, <[_]>::len)
        ));
        let installed = crate::agents::list().len();
        items.push(format!(
            "{:<22}· {installed} specialist(s) installed",
            "Per-agent pin"
        ));
        items.push("Back".to_string());

        let pick = match Select::with_theme(&theme)
            .with_prompt("Sub-agents & roles (Esc when done)")
            .items(&items)
            .default(0)
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        match pick {
            i if i < ROLE_ROWS.len() => {
                let (key, label, what) = ROLE_ROWS[i];
                tui::emit_line(&format!("  {}", style(what).dim()));
                config_edit_one_role(cfg, key, label).await?;
            }
            i if i == ROLE_ROWS.len() => {
                line_warn("advanced model→endpoint overrides can supersede provider-based routing");
                config_edit_model_registry(cfg).await?
            }
            i if i == ROLE_ROWS.len() + 1 => config_edit_agent_pins(cfg).await?,
            _ => return Ok(()),
        }
    }
}

/// Ask for a base URL for a role/registry entry and PROBE it before accepting.
///
/// Returns `(url, models)` — the model list is a by-product of the probe worth keeping, because it
/// turns the next step from "type a model id correctly from memory" into a picker. An empty entry
/// means "inherit the main endpoint" and returns `None`.
///
/// A failed probe does NOT block saving. The endpoint may be down, behind a VPN, or simply not
/// expose `/models`; refusing the value would make this menu unusable in exactly those cases. It
/// warns with the real reason and asks — the same shape `config_edit_connection` uses for an
/// unreachable preset.
async fn prompt_probed_base_url(
    theme: &ColorfulTheme,
    http: &reqwest::Client,
    current: Option<&str>,
    inherit_label: &str,
) -> Result<Option<(String, Vec<client::ModelInfo>)>> {
    let mut input = Input::<String>::with_theme(theme)
        .with_prompt(format!("Base URL (empty = {inherit_label}, `-` clears)"))
        .allow_empty(true);
    if let Some(c) = current {
        input = input.default(c.to_string());
    }
    let raw = input.interact_text()?;
    let raw = raw.trim();
    if raw.is_empty() || raw == "-" {
        return Ok(None);
    }
    let url = raw.trim_end_matches('/').to_string();
    let check = spin_while(
        &format!("checking {url}"),
        client::check_endpoint(http, &url, None),
    )
    .await;
    match check {
        client::EndpointCheck::Ok(infos) => {
            line_ok(&format!("reachable — {} models", infos.len()));
            Ok(Some((url, infos)))
        }
        client::EndpointCheck::Auth(_) => {
            line_ok("reachable (needs a key — next step)");
            Ok(Some((url, Vec::new())))
        }
        other => {
            let what = match &other {
                client::EndpointCheck::NotFound(d) => match missing_version_suffix(&url) {
                    Some(fixed) => format!(
                        "no model list there — most endpoints need a version path, e.g. {fixed}"
                    ),
                    None => format!("no model list there ({d})"),
                },
                client::EndpointCheck::Unreachable(d) => format!("could not reach it ({d})"),
                client::EndpointCheck::Http(c, d) => format!("HTTP {c} ({d})"),
                _ => unreachable!("Ok/Auth handled above"),
            };
            line_warn(&what);
            if yn(theme, "Save this URL anyway?", false)? {
                Ok(Some((url, Vec::new())))
            } else {
                Ok(None)
            }
        }
    }
}

/// Ask how a role/entry should authenticate. `env:VAR` is offered first and by default: it keeps the
/// secret out of `cli-config.json`, and the resolver already understands the indirection.
///
/// Returns `Some(value)` to store, or `None` to inherit the main key.
fn prompt_api_key_ref(theme: &ColorfulTheme, current: Option<&str>) -> Result<Option<String>> {
    let opts = [
        "env:VAR — read from an environment variable (recommended)",
        "paste a key — stored in cli-config.json",
        "inherit the main endpoint's key",
    ];
    let default_idx = match current {
        Some(c) if c.starts_with("env:") => 0,
        Some(_) => 1,
        None => 2,
    };
    let pick = match Select::with_theme(theme)
        .with_prompt("API key (Esc keeps current)")
        .items(&opts)
        .default(default_idx)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(current.map(str::to_string)),
    };
    match pick {
        0 => {
            let mut input = Input::<String>::with_theme(theme).with_prompt("Environment variable");
            if let Some(v) = current.and_then(|c| c.strip_prefix("env:")) {
                input = input.default(v.to_string());
            }
            let var = input.allow_empty(true).interact_text()?;
            let var = var.trim().trim_start_matches("env:").trim();
            if var.is_empty() {
                return Ok(None);
            }
            // Say so now rather than at dispatch time: an unset variable resolves to the inherited
            // key, which looks like the setting silently did nothing.
            if std::env::var(var)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_none()
            {
                line_warn(&format!(
                    "{var} isn't set in this shell — until it is, this role falls back to the main key"
                ));
            }
            Ok(Some(format!("env:{var}")))
        }
        1 => {
            let key: String = Input::with_theme(theme)
                .with_prompt("API key")
                .allow_empty(true)
                .interact_text()?;
            let key = key.trim();
            Ok((!key.is_empty()).then(|| key.to_string()))
        }
        _ => Ok(None),
    }
}

fn prompt_required_provider_model(
    theme: &ColorfulTheme,
    infos: &[client::ModelInfo],
    current: Option<&str>,
) -> Result<Option<String>> {
    if !infos.is_empty() {
        let ids: Vec<String> = infos.iter().map(|m| m.id.clone()).collect();
        let mut items = ids.clone();
        items.push(CUSTOM_MODEL_ITEM.to_string());
        let Some(pick) = Select::with_theme(theme)
            .with_prompt("Provider default model (Esc cancels)")
            .items(&items)
            .default(model_default_index(&ids, current))
            .interact_opt()?
        else {
            return Ok(None);
        };
        if pick < ids.len() {
            return Ok(Some(ids[pick].clone()));
        }
    }
    let mut input = Input::<String>::with_theme(theme)
        .with_prompt("Provider default model id (empty cancels)")
        .allow_empty(true);
    if let Some(model) = current {
        input = input.default(model.to_string());
    }
    let value = input.interact_text()?;
    Ok((!value.trim().is_empty()).then(|| value.trim().to_string()))
}

/// Edit one role by choosing a saved provider and an optional model override. Direct URL/key fields
/// are preserved only as advanced legacy overrides elsewhere.
async fn config_edit_one_role(
    cfg: &mut cli_config::CliConfig,
    role: &str,
    label: &str,
) -> Result<()> {
    let theme = ui_theme();
    if cfg.providers.as_ref().is_none_or(Vec::is_empty) {
        line_warn("no saved providers — add one first");
        config_edit_providers(cfg).await?;
        if cfg.providers.as_ref().is_none_or(Vec::is_empty) {
            return Ok(());
        }
    }
    let cur = role_get(cfg, role).cloned().unwrap_or_default();
    let list = cfg.providers.clone().unwrap_or_default();
    let mut items = vec!["‹ inherit the main provider ›".to_string()];
    items.extend(list.iter().map(|p| provider_row(cfg, p)));
    let default = cur
        .provider
        .as_deref()
        .and_then(|name| list.iter().position(|p| p.matches_name(name)))
        .map(|i| i + 1)
        .unwrap_or(0);
    let Some(pick) = Select::with_theme(&theme)
        .with_prompt(format!("{label} — provider (Esc keeps current)"))
        .items(&items)
        .default(default)
        .interact_opt()?
    else {
        return Ok(());
    };
    let provider = (pick > 0).then(|| list[pick - 1].name.clone());
    let selected = provider
        .as_deref()
        .and_then(|name| cfg.provider(name))
        .cloned();
    let model = if let Some(profile) = selected.as_ref() {
        let options = [
            format!("Use provider default · {}", profile.model),
            "Enter another model id".to_string(),
        ];
        match Select::with_theme(&theme)
            .with_prompt("Model")
            .items(&options)
            .default(if cur.model.is_some() { 1 } else { 0 })
            .interact_opt()?
        {
            Some(0) => None,
            Some(1) => {
                let mut input = Input::<String>::with_theme(&theme).with_prompt("Model id");
                if let Some(m) = cur.model.clone() {
                    input = input.default(m);
                }
                let value = input.allow_empty(true).interact_text()?;
                (!value.trim().is_empty()).then(|| value.trim().to_string())
            }
            None => return Ok(()),
            _ => None,
        }
    } else {
        None
    };
    let mut roles = cfg.roles.take().unwrap_or_default();
    let slot = role_slot(&mut roles, role);
    *slot = (provider.is_some() || model.is_some()).then_some(cli_config::RoleModelConfig {
        provider,
        model,
        ..Default::default()
    });
    cfg.roles = roles.has_any().then_some(roles);
    Ok(())
}

/// Edit the model→endpoint registry: the table that lets a model name carry its own gateway, so a
/// specialist pinned to another provider's model reaches THAT provider.
async fn config_edit_model_registry(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let http = http_client()?;
    loop {
        let list = cfg.model_endpoints.clone().unwrap_or_default();
        let mut items: Vec<String> = list
            .iter()
            .map(|e| {
                format!(
                    "{}  → {}{}",
                    e.model,
                    e.base_url.as_deref().unwrap_or("(caller's url)"),
                    e.api_key_ref
                        .as_deref()
                        .map(|k| if k.starts_with("env:") {
                            format!(" · {k}")
                        } else {
                            " · own key".to_string()
                        })
                        .unwrap_or_default()
                )
            })
            .collect();
        items.push("＋ add a model mapping".to_string());
        items.push("Back".to_string());
        let pick = match Select::with_theme(&theme)
            .with_prompt("Model → endpoint (Esc when done)")
            .items(&items)
            .default(items.len().saturating_sub(2))
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick == items.len() - 1 {
            return Ok(());
        }
        if pick < list.len() {
            let existing = &list[pick];
            let action = match Select::with_theme(&theme)
                .with_prompt(format!("{} (Esc cancels)", existing.model))
                .items(&["edit", "remove"])
                .default(0)
                .interact_opt()?
            {
                Some(a) => a,
                None => continue,
            };
            if action == 1 {
                apply_model_endpoint(cfg, &format!("{},clear", existing.model))?;
                line_ok(&format!("removed {}", existing.model));
                continue;
            }
        }
        // Add, or edit-in-place (same prompts; the model id is pre-filled when editing).
        let editing = (pick < list.len()).then(|| list[pick].clone());
        let mut mi = Input::<String>::with_theme(&theme).with_prompt("Model id");
        if let Some(e) = &editing {
            mi = mi.default(e.model.clone());
        }
        let model = mi.allow_empty(true).interact_text()?;
        let model = model.trim().to_string();
        if model.is_empty() {
            continue;
        }
        let probed = prompt_probed_base_url(
            &theme,
            &http,
            editing.as_ref().and_then(|e| e.base_url.as_deref()),
            "inherit the caller's url",
        )
        .await?;
        let api_key_ref = prompt_api_key_ref(
            &theme,
            editing.as_ref().and_then(|e| e.api_key_ref.as_deref()),
        )?;
        let base_url = probed.map(|(u, _)| u);
        let mut out = cfg.model_endpoints.take().unwrap_or_default();
        out.retain(|e| e.model != model);
        if base_url.is_some() || api_key_ref.is_some() {
            out.push(cli_config::ModelEndpoint {
                model: model.clone(),
                base_url,
                api_key_ref,
            });
            line_ok(&format!("mapped {model}"));
        } else {
            // Both fields empty would be a no-op entry that only adds noise to `config show`.
            line_warn(&format!("{model} has no url or key — entry dropped"));
        }
        cfg.model_endpoints = (!out.is_empty()).then_some(out);
    }
}

/// Assign one installed specialist to a saved provider and optional model override.
async fn config_edit_agent_pins(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let all = crate::agents::list();
    if all.is_empty() {
        line_warn("no specialists installed — `aizen agents install msitarzewski/agency-agents`");
        return Ok(());
    }
    if cfg.providers.as_ref().is_none_or(Vec::is_empty) {
        line_warn("no saved providers — add one first");
        config_edit_providers(cfg).await?;
        if cfg.providers.as_ref().is_none_or(Vec::is_empty) {
            return Ok(());
        }
    }
    loop {
        let items: Vec<String> = all
            .iter()
            .map(|d| {
                let route = cfg.agent_route(&d.slug());
                format!(
                    "{:<24}· {} · {}",
                    d.slug(),
                    route
                        .and_then(|r| r.provider.as_deref())
                        .unwrap_or("inherit sub-agent default"),
                    route
                        .and_then(|r| r.model.as_deref())
                        .unwrap_or("default model")
                )
            })
            .collect();
        let Some(pick) = Select::with_theme(&theme)
            .with_prompt("Specialist agent (Esc when done)")
            .items(&items)
            .default(0)
            .interact_opt()?
        else {
            return Ok(());
        };
        let slug = all[pick].slug();
        let current = cfg.agent_route(&slug).cloned().unwrap_or_default();
        let providers = cfg.providers.clone().unwrap_or_default();
        let mut choices = vec!["‹ inherit sub-agent default ›".to_string()];
        choices.extend(providers.iter().map(|p| provider_row(cfg, p)));
        let default = current
            .provider
            .as_deref()
            .and_then(|name| providers.iter().position(|p| p.matches_name(name)))
            .map(|i| i + 1)
            .unwrap_or(0);
        let Some(pp) = Select::with_theme(&theme)
            .with_prompt(format!("{slug} — provider"))
            .items(&choices)
            .default(default)
            .interact_opt()?
        else {
            continue;
        };
        if pp == 0 {
            cfg.set_agent_route(&slug, None, None)?;
            continue;
        }
        let provider = &providers[pp - 1];
        let model_choices = [
            format!("Use provider default · {}", provider.model),
            "Enter another model id".to_string(),
        ];
        let Some(mp) = Select::with_theme(&theme)
            .with_prompt("Model")
            .items(&model_choices)
            .default(if current.model.is_some() { 1 } else { 0 })
            .interact_opt()?
        else {
            continue;
        };
        let model = if mp == 0 {
            None
        } else {
            let mut input = Input::<String>::with_theme(&theme).with_prompt("Model id");
            if let Some(m) = current.model {
                input = input.default(m);
            }
            let value = input.allow_empty(true).interact_text()?;
            (!value.trim().is_empty()).then(|| value.trim().to_string())
        };
        cfg.set_agent_route(&slug, Some(provider.name.clone()), model)?;
    }
}

/// Section editor: fetch the model list, pick one (Esc keeps current), then the context window.
async fn config_edit_model(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let (base, key) = match (cfg.base_url.clone(), cfg.api_key.clone()) {
        (Some(b), Some(k)) => (b, k),
        _ => {
            tui::emit_line(&format!(
                "  {}",
                style("set the Connection (base URL + key) first").color256(crate::ui::theme::WARN)
            ));
            return Ok(());
        }
    };
    let http = http_client()?;
    // One complete line per outcome rather than a bare `print!` prefix completed later: a partial
    // line cannot be a transcript block, so under the retained renderer it was a raw write into cells
    // the render thread believes it owns (the same corruption `print_config` was fixed for).
    tui::emit_line(
        &style(format!("Fetching models from {base} …"))
            .dim()
            .to_string(),
    );
    match client::fetch_models_info(&http, &base, &key).await {
        Ok(infos) if !infos.is_empty() => {
            tui::emit_line(
                &style(format!("ok ({} found)", infos.len()))
                    .dim()
                    .to_string(),
            );
            let ids: Vec<String> = infos.iter().map(|m| m.id.clone()).collect();
            let mut items: Vec<String> = ids.clone();
            items.push(CUSTOM_MODEL_ITEM.to_string());
            let pick = match Select::with_theme(&theme)
                .with_prompt("Pick a model (Esc keeps current)")
                .items(&items)
                .default(model_default_index(&ids, cfg.model.as_deref()))
                .interact_opt()?
            {
                Some(i) => i,
                None => return Ok(()),
            };
            if pick < infos.len() {
                cfg.model = Some(infos[pick].id.clone());
                cfg.model_context_window = infos[pick].context_length; // auto when reported, else heuristic
            } else {
                let m: String = Input::with_theme(&theme)
                    .with_prompt("Model id")
                    .interact_text()?;
                if !m.trim().is_empty() {
                    cfg.model = Some(m.trim().to_string());
                    cfg.model_context_window = None;
                }
            }
        }
        other => {
            match other {
                Ok(_) => tui::emit_line(&style("no models returned.").dim().to_string()),
                Err(e) => tui::note_line(&style(format!("failed: {e}")).red().to_string()),
            }
            let mut mi =
                Input::<String>::with_theme(&theme).with_prompt("Enter a model id manually");
            if let Some(cur) = cfg.model.clone() {
                mi = mi.default(cur);
            }
            let m = mi.allow_empty(true).interact_text()?;
            if !m.trim().is_empty() {
                cfg.model = Some(m.trim().to_string());
                cfg.model_context_window = None;
            }
        }
    }
    // context window — drives the `% context` HUD + auto-compact trigger.
    if let Some(model) = cfg.model.clone() {
        let (shown, was_cfg) = effective_ctx_window(&model, cfg.model_context_window);
        let ctx_default = cfg
            .model_context_window
            .map(|w| w.to_string())
            .unwrap_or_else(|| "auto".to_string());
        let note = if was_cfg {
            "auto-detected from the provider"
        } else {
            "estimated from the model name"
        };
        tui::emit_line(&format!(
            "{}",
            style(format!(
                "Context window — currently {shown} tokens ({note})."
            ))
            .dim()
        ));
        let ctx_in = Input::<String>::with_theme(&theme)
            .with_prompt("Context window (tokens, e.g. 200000 / 128k, or `auto`)")
            .default(ctx_default)
            .allow_empty(true)
            .interact_text()?;
        cfg.model_context_window = match ctx_in
            .trim()
            .to_ascii_lowercase()
            .replace('_', "")
            .replace('k', "000")
            .parse::<usize>()
        {
            Ok(n) if n >= 1000 => Some(n),
            _ => None, // "auto"/blank/garbage → detect-or-heuristic
        };
    }
    cfg.sync_active_provider();
    Ok(())
}

/// What the user decided about one search key.
enum ReachKeyEdit {
    /// Store this key (already proven, or kept deliberately despite an unverifiable check).
    Set(String),
    /// Remove the stored key (`-`).
    Cleared,
    /// Leave whatever is stored alone (empty entry, or gave up).
    Unchanged,
}

/// Ask for one web-search key and verify it with a real (minimal) search before accepting it.
///
/// `check` runs the provider-specific probe. A REJECTED key re-prompts — that is the whole point of
/// this loop, since a bad search key otherwise sits in the config until the agent's first search
/// fails mid-task. An UNREACHABLE result does not re-prompt by default: the key may be perfectly
/// good and only the network at fault, so blaming the user's paste would be wrong.
///
/// Keys are entered VISIBLY, same reasoning as the API key: a pasted secret you can't see is a
/// truncated paste you can't spot, and it lands in a plaintext config either way.
async fn prompt_validated_reach_key<F, Fut>(
    theme: &ColorfulTheme,
    label: &str,
    keys_url: &str,
    current: Option<&str>,
    check: F,
) -> Result<ReachKeyEdit>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = crate::agent::reach::search::KeyCheck>,
{
    tui::emit_line(&format!(
        "  {}",
        style(format!("get a key: {keys_url}")).dim()
    ));
    loop {
        let prompt = match current {
            Some(k) => format!(
                "{label} key (current {} — Enter keeps, `-` clears)",
                cli_config::mask(k)
            ),
            None => format!("{label} key (Enter to skip)"),
        };
        let entered = Input::<String>::with_theme(theme)
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?;
        let entered = entered.trim().to_string();
        if entered.is_empty() {
            return Ok(ReachKeyEdit::Unchanged);
        }
        if entered == "-" {
            return Ok(ReachKeyEdit::Cleared);
        }

        let verdict = spin_while(
            &format!("verifying the {label} key"),
            check(entered.clone()),
        )
        .await;
        match verdict {
            crate::agent::reach::search::KeyCheck::Ok(n) => {
                line_ok(&format!("{label} key works — {n} results for a test query"));
                return Ok(ReachKeyEdit::Set(entered));
            }
            crate::agent::reach::search::KeyCheck::Rejected(why) => {
                line_bad(&why);
                if !yn(theme, "Enter a different key?", true)? {
                    return Ok(ReachKeyEdit::Unchanged);
                }
            }
            crate::agent::reach::search::KeyCheck::Unreachable(why) => {
                line_warn(&format!("could not verify it — {why}"));
                if yn(theme, "Keep this key anyway?", true)? {
                    return Ok(ReachKeyEdit::Set(entered));
                }
                if !yn(theme, "Enter a different key?", true)? {
                    return Ok(ReachKeyEdit::Unchanged);
                }
            }
        }
    }
}

/// Section editor: the web-search keys (Tavily, and Jina as a fallback), each verified live.
///
/// Both are optional, but `web_search` is KEYED-ONLY: with neither key the tool returns an
/// "add a key" error rather than degrading, so the section says that up front instead of letting the
/// user discover it from a failed search.
async fn config_edit_websearch(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    tui::emit_line(&format!(
        "{}",
        style("web_search is keyed-only: without a key it returns an error rather than guessing.")
            .dim()
    ));
    // Say when the environment is in charge — otherwise editing this and seeing no change is baffling.
    for (var, what) in [
        ("AIZEN_TAVILY_API_KEY", "Tavily"),
        ("TAVILY_API_KEY", "Tavily"),
        ("AIZEN_JINA_API_KEY", "Jina"),
        ("JINA_API_KEY", "Jina"),
    ] {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            line_warn(&format!(
                "${var} is set — it overrides the {what} key saved here"
            ));
        }
    }

    let cur_tavily = cfg.reach.as_ref().and_then(|r| r.tavily_api_key.clone());
    let edit = prompt_validated_reach_key(
        &theme,
        "Tavily",
        "https://app.tavily.com (free tier)",
        cur_tavily.as_deref(),
        |k| async move { crate::agent::reach::search::check_tavily_key(&k).await },
    )
    .await?;
    match edit {
        ReachKeyEdit::Set(k) => {
            cfg.reach
                .get_or_insert_with(Default::default)
                .tavily_api_key = Some(k)
        }
        ReachKeyEdit::Cleared => {
            cfg.reach
                .get_or_insert_with(Default::default)
                .tavily_api_key = None
        }
        ReachKeyEdit::Unchanged => {}
    }

    let cur_jina = cfg.reach.as_ref().and_then(|r| r.jina_api_key.clone());
    // Only worth offering when there's a reason to: as a fallback next to Tavily, or as the only
    // backend when Tavily is absent.
    if cur_jina.is_some()
        || yn(
            &theme,
            "Add a Jina key too? (a search fallback + a better page reader)",
            cur_tavily.is_none(),
        )?
    {
        let edit = prompt_validated_reach_key(
            &theme,
            "Jina",
            "https://jina.ai/reader (free tier)",
            cur_jina.as_deref(),
            |k| async move { crate::agent::reach::search::check_jina_key(&k).await },
        )
        .await?;
        match edit {
            ReachKeyEdit::Set(k) => {
                cfg.reach.get_or_insert_with(Default::default).jina_api_key = Some(k)
            }
            ReachKeyEdit::Cleared => {
                cfg.reach.get_or_insert_with(Default::default).jina_api_key = None
            }
            ReachKeyEdit::Unchanged => {}
        }
    }
    Ok(())
}

/// One-line Memory summary for the hub row: what recall is doing right now.
fn memory_hint(cfg: &cli_config::CliConfig) -> String {
    let learn = if cfg.memory_auto_learn.unwrap_or(true) {
        "auto-learn on"
    } else {
        "auto-learn off"
    };
    // Report the tier that will ACTUALLY run, not the flag: `settings()` already folds in the cargo
    // feature, the env override, and whether a model exists on disk.
    let tier = if memory::settings().enable_dense {
        "lexical + dense"
    } else {
        "lexical"
    };
    format!("{learn} · {tier}")
}

/// Section editor: memory — what gets learned, and which retrieval tiers run.
///
/// The dense half is reported before it is offered, because three independent things decide whether
/// semantic recall runs at all (the `dense` cargo feature, an installed model, `AIZEN_MEM_DENSE`), and
/// a menu that hid that would let someone "pick a model" on a build that can never use one.
fn config_edit_memory(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();

    cfg.memory_auto_learn = Some(yn(
        &theme,
        "Auto-learn durable facts from each turn?",
        cfg.memory_auto_learn.unwrap_or(true),
    )?);

    // ── dense (semantic) recall status ──
    let dense_built = cfg!(feature = "dense");
    let models = memory::embed::list_local_models();
    let active = memory::settings().enable_dense;

    if !dense_built {
        line_warn("this build has no semantic backend — recall is lexical only");
        tui::emit_line(&format!(
            "  {}",
            style("(a `--features dense` build adds embedding-based recall for paraphrases)").dim()
        ));
        return Ok(());
    }
    if let Ok(v) = std::env::var("AIZEN_MEM_DENSE") {
        line_warn(&format!(
            "$AIZEN_MEM_DENSE={v} overrides the dense decision below"
        ));
    }
    if models.is_empty() {
        line_warn("no embedding model installed — dense recall is off");
        tui::emit_line(&format!(
            "  {}",
            style("get one with: aizen memory model-download").dim()
        ));
        return Ok(());
    }
    if active {
        line_ok("dense recall is on");
    } else {
        line_warn("dense recall is off");
    }

    // Which model, out of what is actually on disk. Auto is first so the default choice stays
    // "whatever discovery ranks best" rather than freezing today's pick into the config file.
    let current = cfg.embed_model.clone();
    let mut items = vec![format!(
        "auto — best installed ({})",
        memory::embed::discover_local_model()
            .map(|c| c.name)
            .unwrap_or_else(|| "none".into())
    )];
    for m in &models {
        items.push(format!("{}  ({})", m.name, m.source));
    }
    let default = current
        .as_deref()
        .and_then(|c| models.iter().position(|m| m.name == c).map(|i| i + 1))
        .unwrap_or(0);
    if let Some(pick) = Select::with_theme(&theme)
        .with_prompt("Embedding model (Esc keeps current)")
        .items(&items)
        .default(default)
        .interact_opt()?
    {
        // 0 = auto ⇒ clear the pin so discovery decides again.
        cfg.embed_model = if pick == 0 {
            None
        } else {
            Some(models[pick - 1].name.clone())
        };
        if let Some(name) = cfg.embed_model.clone() {
            if std::env::var("AIZEN_EMBED_MODEL").is_ok_and(|v| !v.trim().is_empty()) {
                line_warn(&format!(
                    "$AIZEN_EMBED_MODEL is set — it overrides this choice of {name}"
                ));
            }
        }
    }
    Ok(())
}

/// Section editor: session behavior — auto-compact %, skill/memory/persona learning, checkpoints.
fn config_edit_session(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let cur_ac = cfg.compact_threshold_pct.unwrap_or(80);
    let ac_default = if cur_ac == 0 {
        "off".to_string()
    } else {
        cur_ac.to_string()
    };
    let ac_in = Input::<String>::with_theme(&theme)
        .with_prompt("Auto-compact at what % of context? (10–95, or `off`)")
        .default(ac_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.compact_threshold_pct = match ac_in.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" => Some(0),
        s => match s.trim_end_matches('%').parse::<u8>() {
            Ok(p) if (10..=95).contains(&p) => Some(p),
            _ => Some(cur_ac),
        },
    };
    cfg.auto_skill_learn = Some(yn(
        &theme,
        "Auto-learn skills from completed tasks?",
        cfg.auto_skill_learn.unwrap_or(true),
    )?);
    // `memory_auto_learn` deliberately lives in the Memory section instead of here: it belongs with
    // the retrieval knobs it feeds, and asking for it twice would let the two prompts disagree.
    cfg.persona_evolve = Some(yn(
        &theme,
        "Persona evolution (learn a voice over time)?",
        cfg.persona_evolve.unwrap_or(true),
    )?);
    let cur_tm = cfg.timemachine_keep.unwrap_or(50);
    let tm_in = Input::<String>::with_theme(&theme)
        .with_prompt("Time-machine checkpoints to keep? (a number, or `unlimited`)")
        .default(if cur_tm == 0 {
            "unlimited".to_string()
        } else {
            cur_tm.to_string()
        })
        .allow_empty(true)
        .interact_text()?;
    cfg.timemachine_keep = match tm_in.trim().to_ascii_lowercase().as_str() {
        "unlimited" | "all" | "0" => Some(0),
        s => match s.parse::<usize>() {
            Ok(n) => Some(n),
            _ => Some(cur_tm),
        },
    };
    Ok(())
}

/// Section editor: reasoning effort tier (arrow-key Select) + the ultimate / adaptive toggles.
fn config_edit_reasoning(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let tiers = [
        "auto (detect per turn)",
        "low",
        "medium",
        "high",
        "xhigh",
        "max",
    ];
    let cur_idx = if cfg.auto_effort == Some(false) {
        match cfg.reasoning_effort.as_deref() {
            Some("low") => 1,
            Some("medium") => 2,
            Some("high") => 3,
            Some("xhigh") => 4,
            Some("max") => 5,
            _ => 0,
        }
    } else {
        0
    };
    let pick = match Select::with_theme(&theme)
        .with_prompt("Reasoning effort (Esc keeps current)")
        .items(&tiers)
        .default(cur_idx)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    if pick == 0 {
        cfg.reasoning_effort = None;
        cfg.auto_effort = None; // back to auto-detect
    } else {
        cfg.reasoning_effort = Some(tiers[pick].to_string());
        cfg.auto_effort = Some(false); // a fixed tier turns auto off
    }
    cfg.ultimate = Some(yn(
        &theme,
        "Ultimate mode (max effort + prefer workflows)?",
        cfg.ultimate.unwrap_or(false),
    )?);
    cfg.adaptive_effort = Some(yn(
        &theme,
        "Adaptive effort (let hard turns climb to xhigh)?",
        cfg.adaptive_effort.unwrap_or(false),
    )?);
    Ok(())
}

fn config_edit_approval(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let modes = [
        "ask — prompt before destructive tools",
        "smart — auto-run read-only shell, prompt for the rest",
        "yolo — pre-authorize tools after the hard safety floor",
    ];
    let current = match cfg.persisted_approval_mode() {
        ApprovalMode::Ask => 0,
        ApprovalMode::Smart => 1,
        ApprovalMode::Yolo => 2,
    };
    if let Some(pick) = Select::with_theme(&theme)
        .with_prompt("Approval level (Esc keeps current)")
        .items(&modes)
        .default(current)
        .interact_opt()?
    {
        cfg.set_approval_mode(match pick {
            1 => ApprovalMode::Smart,
            2 => ApprovalMode::Yolo,
            _ => ApprovalMode::Ask,
        });
    }
    Ok(())
}

/// Section editor: icon style plus final-answer visual structure, both applied on the next turn.
fn config_edit_display(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let opts = ["nerd (needs a Nerd Font)", "emoji (any font)", "off"];
    let cur_idx = match cfg.icons.as_deref().unwrap_or("nerd") {
        "emoji" => 1,
        "off" | "none" => 2,
        _ => 0,
    };
    if let Some(pick) = Select::with_theme(&theme)
        .with_prompt("Icons (Esc keeps current)")
        .items(&opts)
        .default(cur_idx)
        .interact_opt()?
    {
        cfg.icons = Some(
            match pick {
                1 => "emoji",
                2 => "off",
                _ => "nerd",
            }
            .to_string(),
        );
        icons::set_tier(cfg.icons.as_deref());
    }

    let visual_opts = [
        "auto (tables/diagrams when useful)",
        "always (every substantial final reply)",
        "off (prose Markdown only)",
    ];
    let visual_idx = match cfg.response_visuals() {
        cli_config::ResponseVisuals::Auto => 0,
        cli_config::ResponseVisuals::Always => 1,
        cli_config::ResponseVisuals::Off => 2,
    };
    if let Some(pick) = Select::with_theme(&theme)
        .with_prompt("Reply visuals (Esc keeps current)")
        .items(&visual_opts)
        .default(visual_idx)
        .interact_opt()?
    {
        cfg.response_visuals = Some(match pick {
            1 => cli_config::ResponseVisuals::Always,
            2 => cli_config::ResponseVisuals::Off,
            _ => cli_config::ResponseVisuals::Auto,
        });
    }

    // The retained full-frame renderer is the only interactive UI, so there is no backend to pick:
    // on a non-TTY (or if the alternate screen won't open) the plain line-REPL takes over on its own.
    Ok(())
}

/// Guided first-time setup (fresh install): walks Connection → Model → Web search → Behavior →
/// Display in order and saves at the end. `config_wizard` calls this only when no endpoint exists yet.
async fn config_setup_full(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let width = tui::width().clamp(46, 72);
    tui::emit_line("");
    tui::emit_line(&format!(
        "{}",
        style("Aizen · setup").bold().color256(splash::ACCENT)
    ));
    tui::emit_line(&format!(
        "{}",
        style(cli_config::config_path().display()).color256(crate::ui::theme::FAINT)
    ));
    tui::emit_line(&format!(
        "{}",
        style("Enter keeps the shown default at each step · Ctrl-C cancels")
            .color256(crate::ui::theme::FAINT)
    ));
    tui::emit_line(&format!(
        "{}",
        style("─".repeat(width)).color256(crate::ui::theme::ACCENT_DIM)
    ));
    // Group the steps under gold section headers so the flow reads as Connection → Model → Behavior.
    let step = |label: &str| {
        tui::emit_line(&format!(
            "\n{} {}",
            style("◆").color256(splash::ACCENT),
            style(label).color256(splash::ACCENT).bold()
        ));
    };

    step("Connection");
    let http = http_client()?;
    // 1) provider → base URL. A preset carries the right version suffix already; a custom URL is
    //    checked (and re-asked) until it answers as a models endpoint.
    let preset = prompt_provider(&theme, cfg.base_url.as_deref())?;
    let (base, mut infos) = match preset {
        Some(p) => {
            let check = spin_while(
                &format!("checking {}", p.base),
                client::check_endpoint(&http, p.base, None),
            )
            .await;
            match check {
                client::EndpointCheck::Ok(infos) => {
                    line_ok(&format!("reachable — {} models", infos.len()));
                    (p.base.to_string(), infos)
                }
                client::EndpointCheck::Auth(_) => {
                    line_ok("reachable (needs a key — next step)");
                    (p.base.to_string(), Vec::new())
                }
                other => {
                    let what = match &other {
                        client::EndpointCheck::NotFound(d) => format!("no model list there ({d})"),
                        client::EndpointCheck::Unreachable(d) => {
                            format!("could not reach it ({d})")
                        }
                        client::EndpointCheck::Http(c, d) => format!("HTTP {c} ({d})"),
                        _ => unreachable!(),
                    };
                    line_warn(&format!("{} — {what}", p.label));
                    if yn(&theme, "Use this URL anyway?", true)? {
                        (p.base.to_string(), Vec::new())
                    } else {
                        prompt_validated_base_url(&theme, &http, Some(p.base), false)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("base URL is required"))?
                    }
                }
            }
        }
        // `allow_skip: false` — first-run setup cannot proceed without an endpoint, so an empty entry
        // re-asks rather than silently leaving the install unconfigured.
        None => prompt_validated_base_url(&theme, &http, cfg.base_url.as_deref(), false)
            .await?
            .ok_or_else(|| anyhow::anyhow!("base URL is required"))?,
    };
    cfg.base_url = Some(base.clone());

    // 2) API key — verified against the endpoint before it's accepted, and visible while typing so a
    //    truncated paste is obvious.
    let (key, fetched) = prompt_validated_api_key(
        &theme,
        &http,
        &base,
        cfg.api_key.as_deref(),
        preset.map(|p| p.keys_url),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("API key is required"))?;
    cfg.api_key = Some(key);
    if !fetched.is_empty() {
        infos = fetched;
    }

    step("Model & context");
    // 3) pick a model from the list the key check already fetched — no second round-trip.
    if infos.is_empty() {
        line_warn("the endpoint listed no models — enter an id manually");
        let mut mi = Input::<String>::with_theme(&theme).with_prompt("Model id");
        if let Some(s) = cfg
            .model
            .clone()
            .or_else(|| preset.map(|p| p.sample_model.to_string()))
        {
            mi = mi.default(s);
        }
        let m = mi.interact_text().context("reading a model id")?;
        if !m.trim().is_empty() {
            cfg.model = Some(m.trim().to_string());
            cfg.model_context_window = None; // manual id, no provider metadata → heuristic
        }
    } else {
        pick_model_from(&theme, cfg, &infos, preset.map(|p| p.sample_model))?;
    }
    if cfg.model.is_none() {
        anyhow::bail!("a model is required (run `aizen models` to list them)");
    }

    // 4) context window — drives the `% context` HUD + the auto-compact trigger. The model pick
    //    above pre-filled `model_context_window` from the provider when it reported one; show that
    //    (or `auto`) as the default. A number overrides it; `auto` clears back to detect/heuristic.
    let model = cfg.model.clone().unwrap();
    let (shown, was_cfg) = effective_ctx_window(&model, cfg.model_context_window);
    let ctx_default = cfg
        .model_context_window
        .map(|w| w.to_string())
        .unwrap_or_else(|| "auto".to_string());
    let note = if was_cfg {
        "auto-detected from the provider"
    } else {
        "estimated from the model name"
    };
    tui::emit_line(&format!(
        "{}",
        style(format!(
            "Context window — currently {shown} tokens ({note})."
        ))
        .dim()
    ));
    let ctx_in = Input::<String>::with_theme(&theme)
        .with_prompt("Context window (tokens, e.g. 200000 / 128k, or `auto`)")
        .default(ctx_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.model_context_window = match ctx_in
        .trim()
        .to_ascii_lowercase()
        .replace('_', "")
        .replace('k', "000")
        .parse::<usize>()
    {
        Ok(n) if n >= 1000 => Some(n),
        _ => None, // "auto"/blank/garbage → detect-or-heuristic
    };

    // Web search key (Tavily) — web_search is KEYED-ONLY, so without a key it can't search at all.
    // Optional here (Enter skips): a fresh install should be usable before the user has gone and
    // signed up for anything. When a key IS given it gets verified with a real search, same as the
    // section editor.
    step("Web search");
    tui::emit_line(&format!(
        "{}",
        style("Optional. web_search is keyed-only — skip now and add one later with `/config`.")
            .dim()
    ));
    let cur_tavily = cfg.reach.as_ref().and_then(|r| r.tavily_api_key.clone());
    let edit = prompt_validated_reach_key(
        &theme,
        "Tavily",
        "https://app.tavily.com (free tier)",
        cur_tavily.as_deref(),
        |k| async move { crate::agent::reach::search::check_tavily_key(&k).await },
    )
    .await?;
    match edit {
        ReachKeyEdit::Set(k) => {
            cfg.reach
                .get_or_insert_with(Default::default)
                .tavily_api_key = Some(k)
        }
        ReachKeyEdit::Cleared => {
            cfg.reach
                .get_or_insert_with(Default::default)
                .tavily_api_key = None
        }
        ReachKeyEdit::Unchanged => {}
    }

    step("Behavior");
    // 5) auto-compact threshold — % of the window at which older turns get summarized (`off` = 0).
    let cur_ac = cfg.compact_threshold_pct.unwrap_or(80);
    let ac_default = if cur_ac == 0 {
        "off".to_string()
    } else {
        cur_ac.to_string()
    };
    let ac_in = Input::<String>::with_theme(&theme)
        .with_prompt("Auto-compact at what % of context? (10–95, or `off`)")
        .default(ac_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.compact_threshold_pct = match ac_in.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" => Some(0),
        s => match s.trim_end_matches('%').parse::<u8>() {
            Ok(p) if (10..=95).contains(&p) => Some(p),
            _ => Some(cur_ac), // blank/garbage → keep current
        },
    };

    // 6) auto-learn skills — distill completed multi-step tasks into reusable skills.
    let cur_sk = cfg.auto_skill_learn.unwrap_or(true);
    let sk_default = if cur_sk {
        "yes".to_string()
    } else {
        "no".to_string()
    };
    let sk_in = Input::<String>::with_theme(&theme)
        .with_prompt("Auto-learn skills from completed tasks? (yes/no)")
        .default(sk_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.auto_skill_learn = match sk_in.trim().to_ascii_lowercase().as_str() {
        "no" | "n" | "off" | "false" => Some(false),
        "yes" | "y" | "on" | "true" => Some(true),
        _ => Some(cur_sk), // blank/garbage → keep current
    };

    // 7) auto-learn memory — passively learn durable user/project facts from each turn (free).
    let cur_ml = cfg.memory_auto_learn.unwrap_or(true);
    let ml_default = if cur_ml {
        "yes".to_string()
    } else {
        "no".to_string()
    };
    let ml_in = Input::<String>::with_theme(&theme)
        .with_prompt("Auto-learn memory (durable facts) from each turn? (yes/no)")
        .default(ml_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.memory_auto_learn = match ml_in.trim().to_ascii_lowercase().as_str() {
        "no" | "n" | "off" | "false" => Some(false),
        "yes" | "y" | "on" | "true" => Some(true),
        _ => Some(cur_ml), // blank/garbage → keep current
    };

    // 8) time machine — how many code checkpoints to keep before auto-pruning the oldest.
    let cur_tm = cfg.timemachine_keep.unwrap_or(50);
    let tm_in = Input::<String>::with_theme(&theme)
        .with_prompt("Time-machine checkpoints to keep? (a number, or `unlimited`)")
        .default(if cur_tm == 0 {
            "unlimited".to_string()
        } else {
            cur_tm.to_string()
        })
        .allow_empty(true)
        .interact_text()?;
    cfg.timemachine_keep = match tm_in.trim().to_ascii_lowercase().as_str() {
        "unlimited" | "all" | "0" => Some(0),
        s => match s.parse::<usize>() {
            Ok(n) => Some(n),
            _ => Some(cur_tm), // blank/garbage → keep current
        },
    };

    step("Display");
    // 8) icon style — nerd (default; crisp monochrome glyphs, needs a Nerd Font) / emoji (colour,
    //    works on any font) / off. Nerd is the default so the TUI reads as one calm accent palette;
    //    a plain font shows tofu → pick emoji.
    let cur_ic = cfg.icons.clone().unwrap_or_else(|| "nerd".to_string());
    let ic_in = Input::<String>::with_theme(&theme)
        .with_prompt("Icons: nerd (needs a Nerd Font) / emoji (any font) / off")
        .default(cur_ic.clone())
        .allow_empty(true)
        .interact_text()?;
    cfg.icons = match ic_in.trim().to_ascii_lowercase().as_str() {
        "nerd" => Some("nerd".to_string()),
        "off" | "none" => Some("off".to_string()),
        "emoji" => Some("emoji".to_string()),
        _ => Some(cur_ic), // blank/garbage → keep current
    };
    icons::set_tier(cfg.icons.as_deref()); // apply immediately for the "Saved" preview below

    cli_config::save(cfg)?;
    tui::emit_line(&format!(
        "\n{} {}",
        crate::ui::theme::ok("✓"),
        style("Saved.").color256(splash::ACCENT).bold()
    ));
    print_config(cfg);
    tui::emit_line(&format!(
        "{}",
        style("Ready — type a message, or run:  aizen chat -p \"hello\"")
            .color256(crate::ui::theme::FAINT)
    ));
    Ok(())
}

#[cfg(test)]
#[path = "../tests/config_ui.rs"]
mod tests;
