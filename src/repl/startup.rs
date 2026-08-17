//! What happens between launching `aizen` and the first prompt: the identity banner, first-run
//! onboarding, the project-local MCP trust prompt, the id migration, and the update probe.
//!
//! Everything here runs at most once per launch and must degrade quietly: a failed probe or a
//! declined prompt still has to leave a usable REPL.

use crate::core::cli_config;
use crate::core::session_store::{most_recent_session, pretty_session_name};
use crate::features;
use crate::ui::menus::apps_menu;
use crate::ui::{config_ui, splash, theme, tui};
use crate::ui_theme;
use console::style;
use dialoguer::Confirm;

/// One-time prompt when a cloned repo ships project-local MCP servers (`./.aizen/mcp.json`): trust
/// + load them, or dismiss (won't nag again — `aizen mcp trust` re-enables). MCP servers can run
/// commands, hence the explicit gate before auto-arming a stranger's repo.
pub(crate) fn prompt_mcp_trust(server_count: usize) {
    let theme = ui_theme();
    println!(
        "\n{}",
        style(format!(
            "⚠ This repo ships {server_count} MCP tool server(s) (./.aizen/mcp.json)."
        ))
        .color256(crate::ui::theme::WARN)
    );
    println!(
        "{}",
        style("MCP servers can run commands on your machine — only trust repos you trust.")
            .color256(crate::ui::theme::FAINT)
    );
    let ok = Confirm::with_theme(&theme)
        .with_prompt("Trust this repo and load its MCP servers?")
        .default(false)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false);
    if ok {
        let _ = crate::agent::mcp::trust_project();
        println!(
            "{}",
            style("✓ trusted — its tools are now available.").color256(splash::ACCENT)
        );
    } else {
        let _ = crate::agent::mcp::dismiss_project();
        println!(
            "{}",
            style("skipped — run `aizen mcp trust` anytime to enable.")
                .color256(crate::ui::theme::FAINT)
        );
    }
}

/// Whether base URL + API key are already present (via the config file OR the `AIZEN_*`/`NG_*` env
/// vars), so a user who arrives pre-configured (env-only / CI image) is never shown the first-run intro.
fn endpoint_ready() -> bool {
    let cfg = cli_config::load();
    let present = |file: Option<String>, suffix: &str| {
        file.filter(|s| !s.trim().is_empty()).is_some() || cli_config::branded_env(suffix).is_some()
    };
    present(cfg.base_url, "BASE_URL") && present(cfg.api_key, "API_KEY")
}

/// Show the first-run intro when: never onboarded AND no usable endpoint yet. (Either condition alone
/// suppresses it — a returning user, or anyone already configured, skips straight to the menu.)
/// `AIZEN_ONBOARD=1` forces it, so an already-configured user can preview the intro.
pub(crate) fn needs_onboarding() -> bool {
    if cli_config::branded_flag("ONBOARD") {
        return true;
    }
    cli_config::load().onboarded != Some(true) && !endpoint_ready()
}

/// First-run experience for a freshly-downloaded `ng`: a branded welcome, then the setup wizard, then
/// an optional messaging-app connect — finally dropping into the normal chat TUI. Marks `onboarded`
/// up front so it shows exactly once (even if the user Ctrl-C's mid-setup); `aizen config` reruns setup.
pub(crate) async fn first_run_onboarding() {
    // Persist the "seen it" flag immediately so this intro never nags on a later launch.
    let mut cfg = cli_config::load();
    cfg.onboarded = Some(true);
    let _ = cli_config::save(&cfg);

    print!("{}", splash::welcome());
    let theme = ui_theme();
    let proceed = Confirm::with_theme(&theme)
        .with_prompt("Set up your connection now?")
        .default(true)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false);
    if !proceed {
        println!(
            "\n{}",
            style("No problem — run `aizen config` whenever you're ready. Type /help inside for a tour.")
                .color256(crate::ui::theme::FAINT)
        );
        return;
    }

    if let Err(e) = config_ui::config_wizard().await {
        // A cancelled/failed wizard shouldn't abort the launch — fall through into the menu.
        tui::note_line(&format!(
            "{} {e}",
            style("setup:").color256(crate::ui::theme::WARN)
        ));
        eprintln!(
            "{}",
            style("You can finish later with `aizen config`.").color256(crate::ui::theme::FAINT)
        );
        return;
    }

    // Optional: connect a messaging app so the agent can reach the user (off by default — opt-in).
    let connect = Confirm::with_theme(&theme)
        .with_prompt(
            "Connect a messaging app now? (Telegram / Discord / Slack / Webhook — optional)",
        )
        .default(false)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false);
    if connect {
        if let Err(e) = apps_menu().await {
            tui::note_line(&format!(
                "{} {e}",
                style("apps:").color256(crate::ui::theme::WARN)
            ));
        }
    }

    println!(
        "\n{} {}",
        style("✓ You're all set.").color256(splash::ACCENT).bold(),
        style("Type to chat · / for commands · /apps for integrations.")
            .color256(crate::ui::theme::FAINT)
    );
    // Discovery nudge for the specialist library (never auto-installs — just points the way).
    println!(
        "{}",
        style("Tip: add specialist sub-agents with `aizen agents install msitarzewski/agency-agents`.")
            .color256(crate::ui::theme::FAINT)
    );
}

/// The startup identity banner: one line saying which project root + zone slug THIS launch is
/// bound to (the audit's top visibility gap: no surface printed either), plus loud notes when git
/// resolved unusually or a legacy zone from the old slug keying still holds data.
pub(crate) fn identity_banner() -> (String, Vec<String>) {
    let root = crate::core::config::project_root();
    let slug = crate::core::config::project_slug();
    let main = format!(
        "project: {} · zone {slug} · /where for details",
        root.display()
    );
    let mut notes = Vec::new();
    if let Some(note) = crate::core::gitx::resolution_note() {
        notes.push(note);
    }
    if let Some(l) = crate::features::zones::quick_legacy_probe() {
        notes.push(format!(
            "⚠ legacy zone {l} has data — `aizen zone migrate` merges it into {slug}"
        ));
    }
    (main, notes)
}

/// Re-slug memory ids the old ASCII-only slugifier shredded (76% of a measured real store), once per
/// home, before any command reads the store.
///
/// Called from `main` rather than the REPL banner: `aizen memory list` is a CLI path that never
/// builds a banner, so migrating there would have left the two surfaces disagreeing about what an id
/// is — the CLI printing shredded names while the REPL printed readable ones, and `memory show <id>`
/// working with only one of them.
///
/// It renames the user's files without asking, which they chose; it does not do so silently, which
/// they did not. The count and the old→new map's path go to stderr so a piped `memory list` stays
/// machine-readable.
pub(crate) fn run_id_migration_once() {
    if let Some(rep) = crate::memory::migrate_ids::run_once_at_startup() {
        if let Some(n) = rep.notice() {
            eprintln!("{}", style(n).dim());
        }
        for w in rep.warnings.iter().take(3) {
            eprintln!(
                "{} {w}",
                style("⚠ memory id migration:").color256(theme::WARN)
            );
        }
    }
    // Persona self-memory filenames had the same defect, worse in proportion (45 of 89 files on the
    // measured store) and less visible, because `/persona self` renders bodies rather than names.
    // Separate pass, separate per-persona flag: a character created later still gets migrated.
    if let Some(rep) = crate::persona::migrate_stems::run_once_at_startup() {
        if let Some(n) = rep.notice() {
            eprintln!("{}", style(n).dim());
        }
        for w in rep.warnings.iter().take(3) {
            eprintln!(
                "{} {w}",
                style("⚠ persona stem migration:").color256(theme::WARN)
            );
        }
    }
}

/// Startup update housekeeping, shared by both REPL surfaces.
///
/// Sweeps the `.old-*` backups an earlier `/update` left behind (nothing holds them once that
/// process exited), arms the silent 24h check, and surfaces whatever the *previous* check cached —
/// so the notice never costs this launch a network round-trip.
pub(crate) fn startup_update_probe() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            features::update::cleanup_stale_backups(dir, &exe);
        }
    }
    features::update::spawn_background_check();
    if let Some(notice) = features::update::cached_notice() {
        tui::emit_line(&style(notice).dim().to_string());
    }
}

/// The "⟲ last conversation ..." line both REPLs offer at startup, or `None` when this project
/// has nothing saved to come back to.
///
/// Every turn is already autosaved, but nothing on a reopened terminal SAID so - it looked like a
/// blank slate, the transcript sat on disk unmentioned, and the user retyped their context. A
/// session from ANOTHER project is only offered when this one has none, and is labelled so that
/// resuming it stays a visible choice.
pub(crate) fn resume_hint() -> Option<String> {
    let (name, n, origin) = most_recent_session()?;
    let origin_note = origin.map(|o| format!(", {o}")).unwrap_or_default();
    Some(format!(
        "⟲ last conversation “{}” ({n} messages{origin_note}) — /resume to continue it",
        pretty_session_name(&name)
    ))
}
