//! Picking a provider or a model interactively: `/model`, the provider picker, and profile switching.
//!
//! Activating a profile rewrites the root endpoint fields atomically, so the next turn, aside
//! question and health probe all observe the same provider without restarting the REPL.

use crate::core::cli_config;
use crate::core::endpoint::{http_client, resolve_base_key};
use crate::llm::client;
use crate::repl::background::spawn_health_probe_once;
use crate::ui::context_report::resolve_ctx_window;
use crate::ui::{config_ui, splash, tui};
use crate::ui_theme;
use anyhow::{Context, Result};
use console::style;
use dialoguer::Select;

/// Switch to one saved provider profile. Root endpoint fields are updated atomically, so the next
/// turn, aside question, and health probe all see the same provider without restarting the REPL.
pub(crate) fn activate_provider_profile(name: &str) -> Result<cli_config::ProviderProfile> {
    let mut cfg = cli_config::load();
    cfg.activate_provider(name)?;
    let profile = cfg
        .provider(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {name}"))?;
    cli_config::save(&cfg)?;
    tui::set_health(tui::HealthKind::Unknown);
    spawn_health_probe_once();
    Ok(profile)
}

pub(crate) async fn provider_menu() -> Result<Option<cli_config::ProviderProfile>> {
    let mut cfg = cli_config::load();
    let list = cfg.providers.clone().unwrap_or_default();
    let mut items: Vec<String> = list
        .iter()
        .map(|p| config_ui::provider_row(&cfg, p))
        .collect();
    items.push("＋ Add provider".to_string());
    items.push("✎ Manage providers".to_string());
    let default = cfg
        .active_provider
        .as_deref()
        .and_then(|name| list.iter().position(|p| p.matches_name(name)))
        .unwrap_or(0)
        .min(items.len().saturating_sub(1));
    let pick = Select::with_theme(&ui_theme())
        .with_prompt("Provider — choose to switch (Esc keeps current)")
        .items(&items)
        .default(default)
        .interact_opt()?;
    match pick {
        Some(i) if i < list.len() => activate_provider_profile(&list[i].name).map(Some),
        Some(_) => {
            config_ui::config_edit_providers(&mut cfg).await?;
            cli_config::save(&cfg)?;
            Ok(None)
        }
        None => Ok(None),
    }
}

/// `/model` — fetch the provider's models, pick one (arrow-key), persist it. Also captures the
/// context window when the provider reports it (→ a real `% context` HUD; else a name heuristic).
pub(crate) async fn slash_model(model_label: &mut String) -> Result<()> {
    let (base, key) = resolve_base_key(None, None)?;
    let http = http_client()?;
    let infos = client::fetch_models_info(&http, &base, &key)
        .await
        .context("fetching models")?;
    if infos.is_empty() {
        anyhow::bail!("the provider returned no models");
    }
    let ids: Vec<String> = infos.iter().map(|m| m.id.clone()).collect();
    // Picker items double as the listing: show each model's context window when the provider
    // reports one (this is why `/model` subsumes the old `/models` — list + pick in one screen).
    let items: Vec<String> = infos
        .iter()
        .map(|m| match m.context_length {
            Some(n) if n >= 1000 => format!("{}  ·  ctx {}K", m.id, n / 1000),
            Some(n) => format!("{}  ·  ctx {n}", m.id),
            None => m.id.clone(),
        })
        .collect();
    let theme = ui_theme();
    let idx = config_ui::model_default_index(&ids, cli_config::load().model.as_deref());
    let prompt = format!(
        "Model ({} available, ↑/↓ to pick, Esc to cancel)",
        infos.len()
    );
    let pick = match Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(&items)
        .default(idx)
        .interact_opt()?
    {
        Some(i) => i,
        None => {
            println!("{}", style("(kept current model)").dim());
            return Ok(());
        }
    };
    let chosen = &infos[pick];
    let mut cfg = cli_config::load();
    cfg.model = Some(chosen.id.clone());
    cfg.model_context_window = chosen.context_length; // Some ⇒ auto; None ⇒ HUD falls back to heuristic
    cli_config::save(&cfg)?;
    *model_label = chosen.id.clone();
    // Re-pin: the save above sets the default for the NEXT window, the pin makes the switch stick in
    // THIS one. Without it the startup pin would win and the user's pick would be ignored.
    cli_config::pin_session_model(&chosen.id);
    let (window, auto) = resolve_ctx_window(&chosen.id);
    let winlabel = if window >= 1000 {
        format!("{}K", window / 1000)
    } else {
        window.to_string()
    };
    let src = if auto { "auto" } else { "est" };
    println!(
        "{}",
        style(format!("model → {}  ·  ctx {winlabel} ({src})", chosen.id)).color256(splash::ACCENT)
    );
    Ok(())
}
