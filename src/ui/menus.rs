//! Every interactive menu the CLI opens: connected apps, skills, personas, and the Telegram /
//! Discord integrations.
//!
//! These all drive the terminal directly through `dialoguer`, which is what separates them from the
//! rest of the REPL: the retained frame must SUSPEND before any of them runs, which is why each one
//! sits behind a command whose row in [`crate::features::slash`] says `stdin: Stdin::Always`.
//!
//! Grouped by the surface they configure. If this file keeps growing, the natural next cut is one
//! module per group (integrations / skills / personas): they share only the small input helpers.

use crate::agent::app_catalog;
use crate::channels::notify;
use crate::cli_args::{DiscordCmd, TelegramCmd};
use crate::core::cli_config;
use crate::core::types::Message;
use crate::hostbot::platforms::{discord, telegram};
use crate::persona;
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::{icons, splash, tui};
use crate::*;
use anyhow::Result;
use console::style;
use dialoguer::{Confirm, Input, Password, Select};

// ───────────────────────────── discord bot daemon + setup ─────────────────────────────

pub(crate) async fn run_discord(cmd: DiscordCmd) -> Result<()> {
    match cmd {
        DiscordCmd::Setup => discord_setup().await,
        DiscordCmd::Test => discord_test().await,
        DiscordCmd::Serve => hostbot::run_discord_serve().await,
        DiscordCmd::Show => {
            discord_status();
            Ok(())
        }
        DiscordCmd::Disable => discord_disable(),
    }
}

async fn discord_test() -> Result<()> {
    let (client, _) =
        discord::configured().context("Discord bot not set up — run `aizen discord setup`")?;
    let name = client.get_me().await?;
    println!(
        "{}",
        style(format!("✓ bot token valid — @{name}")).color256(splash::ACCENT)
    );
    Ok(())
}

fn discord_status() {
    let d = cli_config::load().discord.unwrap_or_default();
    let token = d
        .resolved_token()
        .map(|t| cli_config::mask(&t))
        .unwrap_or_else(|| "not set".to_string());
    println!("{}", style("Discord bot").bold().color256(splash::ACCENT));
    println!("token:    {token}");
    println!("channels: {:?}", d.allowed_channel_ids);
    if !d.allowed_user_ids.is_empty() {
        println!("users:    {:?}", d.allowed_user_ids);
    }
    println!(
        "configured: {}",
        if discord::is_configured() {
            "yes"
        } else {
            "no"
        }
    );
}

fn discord_disable() -> Result<()> {
    let mut cfg = cli_config::load();
    if cfg.discord.is_none() {
        println!("(Discord bot was not configured)");
        return Ok(());
    }
    cfg.discord = None;
    cli_config::save(&cfg)?;
    println!(
        "{}",
        style("Discord bot disabled (config removed).").color256(splash::ACCENT)
    );
    Ok(())
}

/// Interactive Discord setup: paste the bot token (validated via /users/@me), then the channel id(s)
/// the bot may respond in.
async fn discord_setup() -> Result<()> {
    let theme = ui_theme();
    println!(
        "\n{}",
        style("Discord bot setup").bold().color256(splash::ACCENT)
    );
    println!(
        "{}",
        style("Create an app + bot at discord.com/developers, ENABLE the \"Message Content Intent\", invite \
               it to your server, copy the bot token.")
            .dim()
    );

    let mut cfg = cli_config::load();
    let mut d = cfg.discord.clone().unwrap_or_default();
    let cur = d
        .token
        .as_deref()
        .map(cli_config::mask)
        .unwrap_or_else(|| "none".to_string());
    let entered = Password::with_theme(&theme)
        .with_prompt(format!("Bot token (current {cur} — Enter to keep)"))
        .allow_empty_password(true)
        .interact()
        .context("reading token")?;
    if !entered.trim().is_empty() {
        d.token = Some(entered.trim().to_string());
    }
    let token = d.token.clone().context("a bot token is required")?;
    let client = discord::Client::new(token)?;
    let name = client
        .get_me()
        .await
        .context("Discord rejected the token — check it and retry")?;
    println!(
        "{}",
        style(format!("✓ bot @{name}")).color256(splash::ACCENT)
    );

    let cur_ch = if d.allowed_channel_ids.is_empty() {
        String::new()
    } else {
        d.allowed_channel_ids
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let chans: String = Input::with_theme(&theme)
        .with_prompt(
            "Allowed channel id(s), comma-separated (right-click a channel → Copy Channel ID)",
        )
        .with_initial_text(cur_ch)
        .allow_empty(true)
        .interact_text()
        .context("reading channel ids")?;
    let ids: Vec<u64> = chans
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect();
    if !ids.is_empty() {
        d.allowed_channel_ids = ids;
    }
    if d.allowed_channel_ids.is_empty() {
        anyhow::bail!("at least one allowed channel id is required (the bot is deny-by-default)");
    }

    cfg.discord = Some(d);
    cli_config::save(&cfg)?;
    println!(
        "\n{}",
        style("Saved. Start the bot with:  aizen discord serve").color256(splash::ACCENT)
    );
    Ok(())
}

pub(crate) async fn run_telegram(cmd: TelegramCmd) -> Result<()> {
    match cmd {
        TelegramCmd::Setup => telegram_setup().await,
        TelegramCmd::Test => telegram_test().await,
        TelegramCmd::Show => telegram_status().await,
    }
}

/// Send a one-off test message to the first allowed chat.
async fn telegram_test() -> Result<()> {
    let (client, cfg) =
        telegram::configured().context("Telegram not set up — choose Set up first")?;
    let chat = telegram::first_chat(&cfg).context("no allowed chat id — re-run Set up")?;
    client
        .send_message(chat, "✅ Aizen test message — Telegram is wired up.")
        .await?;
    println!(
        "{}",
        style(format!("sent a test message to chat {chat}")).color256(splash::ACCENT)
    );
    Ok(())
}

/// Print the Telegram integration status (token masked, bot name, allowed chats, daemon state).
async fn telegram_status() -> Result<()> {
    let tg = cli_config::load().telegram.unwrap_or_default();
    match tg.resolved_token() {
        Some(t) => {
            println!("token:    {}", cli_config::mask(&t));
            if let Ok(client) = telegram::Client::new(t) {
                match client.get_me().await {
                    Ok(name) => println!("bot:      @{name}"),
                    Err(_) => println!("bot:      (token present but getMe failed — check it)"),
                }
            }
        }
        None => println!("token:    (unset)"),
    }
    println!("chat ids: {:?}", tg.allowed_chat_ids);
    println!(
        "daemon:   {}",
        if telegram::daemon_is_active() {
            "running (this process)"
        } else {
            "stopped"
        }
    );
    Ok(())
}

/// Remove the Telegram bot config (token + allowed chats).
fn telegram_disable() -> Result<()> {
    let mut cfg = cli_config::load();
    if cfg.telegram.is_none() {
        println!("{}", style("(Telegram was not configured)").dim());
        return Ok(());
    }
    cfg.telegram = None;
    cli_config::save(&cfg)?;
    println!(
        "{}",
        style("Telegram disabled (bot config removed).").color256(splash::ACCENT)
    );
    Ok(())
}

/// An Aizen "connected app" surfaced in the `/apps` hub. Telegram is two-way (a long-poll daemon +
/// approval buttons); the rest are one-way outbound POST channels (see `notify.rs`). **To add a
/// POST-style app**: add a `notify::Channel` variant — it appears here automatically. **To add a
/// richer two-way app**: add an `Integration` variant + arms in the methods below + its `*_menu()`.
#[derive(Clone, Copy)]
enum Integration {
    AppCatalog,
    Telegram,
    Discord,
    Notify(notify::Channel),
}

impl Integration {
    const ALL: &'static [Integration] = &[
        Integration::AppCatalog,
        Integration::Telegram,
        Integration::Discord,
        Integration::Notify(notify::Channel::Slack),
        Integration::Notify(notify::Channel::Webhook),
    ];

    fn name(&self) -> &'static str {
        match self {
            Integration::AppCatalog => "Connect an app",
            Integration::Telegram => "Telegram",
            Integration::Discord => "Discord",
            Integration::Notify(c) => c.label(),
        }
    }
    fn blurb(&self) -> &'static str {
        match self {
            Integration::AppCatalog => "GitHub · Notion · Slack · Linear · Spotify · Google (via MCP)",
            Integration::Telegram => "control aizen from your phone (bot + approval prompts)",
            Integration::Discord => "two-way bot (chat + run the agent; no approval prompts yet) and/or one-way notify webhook",
            Integration::Notify(c) => c.blurb(),
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            Integration::AppCatalog => "🧩",
            Integration::Telegram => "📱",
            Integration::Discord => "🎮",
            Integration::Notify(c) => c.icon(),
        }
    }
    fn configured(&self) -> bool {
        match self {
            Integration::AppCatalog => !app_catalog::installed_keys().is_empty(),
            Integration::Telegram => telegram::is_configured(),
            // Discord counts as configured if EITHER the two-way bot or the notify webhook is set.
            Integration::Discord => {
                discord::is_configured() || notify::is_configured(notify::Channel::Discord)
            }
            Integration::Notify(c) => notify::is_configured(*c),
        }
    }
    async fn open(&self) -> Result<()> {
        match self {
            Integration::AppCatalog => app_catalog_menu().await,
            Integration::Telegram => telegram_menu().await,
            Integration::Discord => discord_app_menu().await,
            Integration::Notify(c) => webhook_app_menu(*c).await,
        }
    }
}

/// `/apps → Connect an app` — pick a featured app (GitHub/Notion/Slack/…) to connect, or search the
/// full MCP registry. Each connect prompts (hidden) for the app's declared token and writes mcp.json.
async fn app_catalog_menu() -> Result<()> {
    let theme = ui_theme();
    let installed = app_catalog::installed_keys();

    // Rows: featured apps first, then any connected custom apps (added via `aizen apps add <name>`).
    struct Row {
        key: String,
        label: String,
        icon: String,
        connected: bool,
        featured: bool,
    }
    let mut rows: Vec<Row> = app_catalog::FEATURED
        .iter()
        .map(|f| Row {
            key: f.key.to_string(),
            label: f.label.to_string(),
            icon: f.icon.to_string(),
            connected: installed.iter().any(|k| k == f.key),
            featured: true,
        })
        .collect();
    for k in &installed {
        if !app_catalog::FEATURED.iter().any(|f| f.key == *k) {
            rows.push(Row {
                key: k.clone(),
                label: k.clone(),
                icon: "🧩".to_string(),
                connected: true,
                featured: false,
            });
        }
    }

    let mut items: Vec<String> = rows
        .iter()
        .map(|r| {
            let badge = if r.connected {
                style("✓").color256(splash::ACCENT).to_string()
            } else {
                style("○").dim().to_string()
            };
            let blurb = if r.featured {
                app_catalog::featured(&r.key).map(|f| f.blurb).unwrap_or("")
            } else {
                "connected (custom)"
            };
            let action = if r.connected {
                style("manage").color256(splash::ACCENT).to_string()
            } else {
                style(blurb).dim().to_string()
            };
            format!(
                "{badge}  {} {}  —  {}",
                icons::g(r.icon.as_str()),
                r.label,
                action
            )
        })
        .collect();
    items.push(format!("{}  Search the full registry…", icons::g("🔎")));
    items.push("Back".to_string());

    let pick = match Select::with_theme(&theme)
        .with_prompt("Apps — pick one (✓ = connected → manage; ○ → connect). Esc to go back")
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };

    if let Some(r) = rows.get(pick) {
        if r.connected {
            return apps_manage_menu(&r.key, &r.label).await;
        }
        return apps_add(&r.key).await;
    }
    if pick == rows.len() {
        // Search flow → hand the query to `apps_add`, which presents the candidate picker (publisher
        // + local/hosted) + secret prompts + confirm gate. One code path, no double-picking.
        let q: String = Input::with_theme(&theme)
            .with_prompt("Search the MCP registry for")
            .allow_empty(true)
            .interact_text()?;
        if q.trim().is_empty() {
            return Ok(());
        }
        return apps_add(q.trim()).await;
    }
    Ok(())
}

/// Manage a CONNECTED app from the TUI: inspect (config + live tools), test the connection live, or
/// disconnect (with confirm). The connect/preview path is `apps_add`; this is its post-connect twin.
async fn apps_manage_menu(key: &str, label: &str) -> Result<()> {
    let theme = ui_theme();
    // OAuth apps get a "Sign in again" action (re-auth / first sign-in if it didn't finish at add).
    let is_oauth = app_catalog::installed_entry(key)
        .and_then(|e| e.get("auth").and_then(|v| v.as_str()).map(|s| s == "oauth"))
        .unwrap_or(false);
    let mut items: Vec<&str> = vec!["View details & tools", "Test connection"];
    if is_oauth {
        items.push("Sign in again (OAuth)");
    }
    items.push("Disconnect");
    items.push("Back");
    let pick = match Select::with_theme(&theme)
        .with_prompt(format!("{label} — connected (Esc to go back)"))
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match items[pick] {
        "View details & tools" => apps_info(key).await,
        "Test connection" => {
            println!(
                "{}",
                style(format!("testing '{key}' (connect + tools/list)…")).dim()
            );
            match crate::agent::mcp::probe(key).await {
                Ok(rep) => println!(
                    "{}",
                    style(format!(
                        "✓ '{key}' connected — {} tool(s) available.",
                        rep.tools.len()
                    ))
                    .color256(splash::ACCENT)
                ),
                Err(e) => println!("{}", style(format!("✗ '{key}' failed — {e:#}")).red()),
            }
            Ok(())
        }
        "Sign in again (OAuth)" => {
            match crate::agent::mcp::login(key).await {
                Ok(()) => println!(
                    "{}",
                    style(format!(
                        "✓ signed in to '{key}'. Takes effect on your next message."
                    ))
                    .color256(splash::ACCENT)
                ),
                Err(e) => println!("{}", style(format!("✗ sign-in failed — {e:#}")).red()),
            }
            Ok(())
        }
        "Disconnect" => {
            let yes = Confirm::with_theme(&theme)
                .with_prompt(format!("Disconnect '{key}'?"))
                .default(false)
                .interact()?;
            if yes {
                if app_catalog::remove_server(key)? {
                    crate::agent::mcp_oauth::clear_token(key); // drop any cached OAuth token too
                    crate::agent::mcp::invalidate();
                    println!(
                        "{}",
                        style(format!(
                            "✓ disconnected '{key}'. Takes effect on your next message."
                        ))
                        .color256(splash::ACCENT)
                    );
                } else {
                    println!("{}", style(format!("'{key}' was not present.")).dim());
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `/apps → Discord` — Discord can be a two-way BOT (receives + replies, needs `aizen discord serve`)
/// and/or a one-way notify WEBHOOK (fire-and-forget alerts). One menu offers both.
async fn discord_app_menu() -> Result<()> {
    let theme = ui_theme();
    let bot = if discord::is_configured() {
        "bot ✓"
    } else {
        "bot ○"
    };
    let hook = if notify::is_configured(notify::Channel::Discord) {
        "webhook ✓"
    } else {
        "webhook ○"
    };
    let items = [
        "Set up two-way bot  (token + channel id)",
        "Test the bot token",
        "Start the bot daemon  (aizen discord serve — Ctrl-C to stop)",
        "Set up one-way notify webhook",
        "Disable the bot",
        "Back",
    ];
    let pick = match Select::with_theme(&theme)
        .with_prompt(format!("Discord — {bot} · {hook} (Esc to go back)"))
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match pick {
        0 => discord_setup().await,
        1 => discord_test().await,
        2 => hostbot::run_discord_serve().await,
        3 => webhook_app_setup(notify::Channel::Discord).await,
        4 => discord_disable(),
        _ => Ok(()),
    }
}

/// `/apps` — the integrations hub: Aizen's connected apps (Telegram today; Discord/Slack/webhooks
/// can slot in via the `Integration` enum). Lists each with a status badge, opens its sub-menu.
pub(crate) async fn apps_menu() -> Result<()> {
    let theme = ui_theme();
    let mut items: Vec<String> = Integration::ALL
        .iter()
        .map(|i| {
            let badge = if i.configured() {
                style("✓").color256(splash::ACCENT).to_string()
            } else {
                style("○").dim().to_string()
            };
            format!(
                "{badge}  {}{}  —  {}",
                icons::g(i.icon()),
                i.name(),
                style(i.blurb()).dim()
            )
        })
        .collect();
    items.push("Back".to_string());
    let pick = match Select::with_theme(&theme)
        .with_prompt("Apps & integrations (Esc to go back)")
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match Integration::ALL.get(pick) {
        Some(app) => app.open().await,
        None => Ok(()), // "Back"
    }
}

/// Menu for a one-way outbound app (Discord / Slack / generic webhook): set the URL, send a test,
/// or disable. Telegram has its own richer menu (it's two-way with a daemon).
async fn webhook_app_menu(ch: notify::Channel) -> Result<()> {
    let theme = ui_theme();
    let configured = notify::is_configured(ch);
    let status = if configured {
        "configured"
    } else {
        "not set up"
    };
    let items: Vec<&str> = if configured {
        vec![
            "Set / update URL",
            "Send a test notification",
            "Disable  (remove the URL)",
            "Back",
        ]
    } else {
        vec!["Set up  (paste the webhook URL)", "Back"]
    };
    let pick = match Select::with_theme(&theme)
        .with_prompt(format!("{} — {status} (Esc to go back)", ch.label()))
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match (configured, pick) {
        (_, 0) => webhook_app_setup(ch).await,
        (true, 1) => webhook_app_test(ch).await,
        (true, 2) => webhook_app_disable(ch),
        _ => Ok(()), // "Back"
    }
}

/// Paste/replace the webhook URL for an outbound app (+ an optional auth header for the generic
/// webhook), persist it, then send a confirmation notification.
async fn webhook_app_setup(ch: notify::Channel) -> Result<()> {
    let theme = ui_theme();
    println!(
        "\n{}",
        style(format!("{} setup", ch.label()))
            .bold()
            .color256(splash::ACCENT)
    );
    println!("{}", style(ch.setup_hint()).dim());

    let mut cfg = cli_config::load();
    let mut n = cfg.notify.clone().unwrap_or_default();
    let cur = notify::channel_url(ch, &cfg)
        .map(|u| cli_config::mask(&u))
        .unwrap_or_else(|| "none".to_string());
    let entered: String = Input::with_theme(&theme)
        .with_prompt(format!(
            "{} URL (current {cur} — Enter to keep)",
            ch.label()
        ))
        .allow_empty(true)
        .interact_text()
        .context("reading URL")?;
    let entered = entered.trim().to_string();
    if !entered.is_empty() {
        if !entered.starts_with("http://") && !entered.starts_with("https://") {
            anyhow::bail!("that doesn't look like a URL (must start with http:// or https://)");
        }
        notify::set_channel_url(&mut n, ch, Some(entered));
    }
    if ch == notify::Channel::Webhook {
        let cur_auth = n
            .webhook_auth
            .as_deref()
            .map(cli_config::mask)
            .unwrap_or_else(|| "none".to_string());
        let auth: String = Input::with_theme(&theme)
            .with_prompt(format!(
                "Auth header — e.g. 'Authorization: Bearer …' (current {cur_auth} — Enter to skip)"
            ))
            .allow_empty(true)
            .interact_text()
            .context("reading auth header")?;
        let auth = auth.trim();
        if !auth.is_empty() {
            n.webhook_auth = Some(auth.to_string());
        }
    }
    cfg.notify = Some(n);
    cli_config::save(&cfg)?;
    println!("{}", style("Saved.").color256(splash::ACCENT));
    if notify::is_configured(ch) {
        println!("{}", style("Sending a test notification…").dim());
        match notify::send_to(
            ch,
            "✅ Aizen connected — this channel will receive agent notifications.",
        )
        .await
        {
            Ok(()) => println!(
                "{}",
                style(format!("✓ test delivered to {}", ch.label())).color256(splash::ACCENT)
            ),
            Err(e) => println!("{}", style(format!("✗ test failed: {e}")).red()),
        }
    }
    Ok(())
}

/// Send a one-off test notification to a configured outbound app.
async fn webhook_app_test(ch: notify::Channel) -> Result<()> {
    println!(
        "{}",
        style(format!("Sending a test notification to {}…", ch.label())).dim()
    );
    match notify::send_to(ch, "🔔 Aizen test notification.").await {
        Ok(()) => println!(
            "{}",
            style(format!("✓ delivered to {}", ch.label())).color256(splash::ACCENT)
        ),
        Err(e) => println!("{}", style(format!("✗ failed: {e}")).red()),
    }
    Ok(())
}

/// Remove an outbound app's stored URL (an env override, if any, still applies — that's intentional).
fn webhook_app_disable(ch: notify::Channel) -> Result<()> {
    let mut cfg = cli_config::load();
    if let Some(n) = cfg.notify.as_mut() {
        notify::set_channel_url(n, ch, None);
        if ch == notify::Channel::Webhook {
            n.webhook_auth = None;
        }
    }
    cli_config::save(&cfg)?;
    println!(
        "{}",
        style(format!("{} disabled (URL removed).", ch.label())).color256(splash::ACCENT)
    );
    Ok(())
}

/// Read multi-line input until a line containing only `.` (used to author a skill body in the REPL).
fn read_multiline_until_dot() -> Result<String> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut lines = Vec::new();
    for line in stdin.lock().lines() {
        let line = line.context("reading input")?;
        if line.trim() == "." {
            break;
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

/// Author a new skill interactively (name + description + trigger + multi-line steps).
fn skill_new_interactive() -> Result<()> {
    let theme = ui_theme();
    let name: String = Input::with_theme(&theme)
        .with_prompt("Skill name")
        .interact_text()?;
    if name.trim().is_empty() {
        anyhow::bail!("a skill name is required");
    }
    let description: String = Input::with_theme(&theme)
        .with_prompt("Description (one line)")
        .allow_empty(true)
        .interact_text()?;
    let when: String = Input::with_theme(&theme)
        .with_prompt("When does it apply? (trigger hint)")
        .allow_empty(true)
        .interact_text()?;
    println!(
        "{}",
        style("Steps — type the procedure; end with a line containing only '.'").dim()
    );
    let body = read_multiline_until_dot()?;
    if body.trim().is_empty() {
        anyhow::bail!("the steps are required");
    }
    let path = skill::save(&name, &description, &when, &body)?;
    println!(
        "{}",
        style(format!("saved skill → {}", path.display())).color256(splash::ACCENT)
    );
    Ok(())
}

/// Prompt for a URL and fetch a skill from it.
async fn skill_fetch_interactive() -> Result<()> {
    let theme = ui_theme();
    let url: String = Input::with_theme(&theme)
        .with_prompt("Skill URL (raw markdown, e.g. a gist/raw GitHub link)")
        .interact_text()?;
    if url.trim().is_empty() {
        anyhow::bail!("a URL is required");
    }
    run_skill_fetch(url.trim(), None).await
}

/// Pick a skill to retire (Esc cancels). Soft — the copy is archived, so `→ Restore` undoes it.
fn skill_delete_interactive(skills: &[skill::Skill]) {
    let theme = ui_theme();
    // Show usage in the picker too: the whole question at this prompt is "which one is dead weight",
    // and a list of bare names cannot answer it. Cold skills sort FIRST for the same reason.
    let mut order: Vec<&skill::Skill> = skills.iter().collect();
    order.sort_by_key(|s| s.uses);
    let names: Vec<String> = order.iter().map(|s| s.name.clone()).collect();
    let labels: Vec<String> = order
        .iter()
        .map(|s| {
            let uses = if s.uses > 0 {
                format!("loaded {}×", s.uses)
            } else {
                "never loaded".into()
            };
            format!("{}  {}", s.name, style(uses).dim())
        })
        .collect();
    if let Ok(Some(i)) = Select::with_theme(&theme)
        .with_prompt("Retire which skill? (archived, not erased — Esc to cancel)")
        .items(&labels)
        .default(0)
        .interact_opt()
    {
        match skill::delete(&names[i]) {
            Ok(true) => println!(
                "{}",
                style(format!(
                    "retired '{}' — restorable from this menu",
                    names[i]
                ))
                .color256(splash::ACCENT)
            ),
            Ok(false) => println!("{}", style("(already gone)").dim()),
            Err(e) => tui::note_line(&format!("{} {e}", style("skill:").red())),
        }
    }
}

/// `/skills` — manage saved procedures: list, view (prints the steps), author a new one, delete.
/// Skills are how-to playbooks the agent loads on demand (distinct from memory = facts).
pub(crate) async fn skills_menu() -> Result<()> {
    loop {
        let theme = ui_theme();
        let skills = skill::list();
        let n = skills.len();
        // Align the name column and carry the two facts that decide keep/fix/drop: where the skill
        // lives (a zone skill is invisible in other repos) and whether it has ever been loaded. The
        // old `name — description` rows were ragged and silent on both.
        let namew = skills
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(34);
        let mut items: Vec<String> = skills
            .iter()
            .map(|s| {
                let d = if s.description.is_empty() {
                    s.when.clone()
                } else {
                    s.description.clone()
                };
                let origin = match s.origin {
                    skill::SkillOrigin::Global => "",
                    skill::SkillOrigin::Project => " [project]",
                    skill::SkillOrigin::Repo => " [repo]",
                };
                let uses = if s.uses > 0 {
                    format!("{}×", s.uses)
                } else {
                    "cold".into()
                };
                let pad = " ".repeat(namew.saturating_sub(s.name.chars().count()));
                format!(
                    "{}{pad}  {}",
                    s.name,
                    style(format!("{uses:<5} {}{origin}", elide(&d, 64))).dim()
                )
            })
            .collect();
        items.push("+ New skill".to_string());
        items.push("⬇ Fetch from URL".to_string());
        items.push(format!(
            "🔎 Search agentskill.sh  {}",
            style("(marketplace)").dim()
        ));
        if n > 0 {
            items.push("✗ Retire a skill".to_string());
        }
        let retired = skill::list_archive();
        if !retired.is_empty() {
            items.push(format!(
                "↩ Restore a retired skill  {}",
                style(format!("({})", retired.len())).dim()
            ));
        }
        items.push("Back".to_string());
        let cold = skills.iter().filter(|s| s.uses == 0).count();
        let prompt = if cold > 0 {
            format!("Skills — {n} saved, {cold} never loaded (Enter to read · Esc to go back)")
        } else {
            format!("Skills — {n} saved (Enter to read · Esc to go back)")
        };
        let pick = match Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(&items)
            .default(0)
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            println!("\n{}", style(skill::render_loaded(&skills[pick])).dim()); // view, then loop
        } else if pick == n {
            if let Err(e) = skill_new_interactive() {
                tui::note_line(&format!("{} {e}", style("skill:").red()));
            }
        } else if pick == n + 1 {
            if let Err(e) = skill_fetch_interactive().await {
                tui::note_line(&format!("{} {e}", style("skill:").red()));
            }
        } else if pick == n + 2 {
            if let Err(e) = skill_search_interactive().await {
                tui::note_line(&format!("{} {e}", style("skill:").red()));
            }
        } else if n > 0 && pick == n + 3 {
            skill_delete_interactive(&skills);
        } else if !retired.is_empty() && pick == n + 3 + usize::from(n > 0) {
            skill_restore_interactive(&retired);
        } else {
            return Ok(()); // Back
        }
    }
}

/// `/skills → Restore` — bring a retired skill back. Its counterpart is the soft retire above;
/// without a restore path in the REPL the archive would only be reachable by hand-moving files.
fn skill_restore_interactive(retired: &[skill::Skill]) {
    let theme = ui_theme();
    let names: Vec<String> = retired.iter().map(|s| s.name.clone()).collect();
    let Ok(Some(i)) = Select::with_theme(&theme)
        .with_prompt("Restore which retired skill? (Esc to cancel)")
        .items(&names)
        .default(0)
        .interact_opt()
    else {
        return;
    };
    match skill::restore(&names[i]) {
        Ok(_) => println!(
            "{}",
            style(format!("restored '{}'", names[i])).color256(splash::ACCENT)
        ),
        Err(e) => tui::note_line(&format!("{} {e}", style("skill:").red())),
    }
}

/// Search agentskill.sh, pick a result, and install it (the interactive `/skills → Search` path).
async fn skill_search_interactive() -> Result<()> {
    let theme = ui_theme();
    let query: String = Input::with_theme(&theme)
        .with_prompt(format!(
            "Search {} for a skill",
            skill_registry::registry_base()
        ))
        .interact_text()
        .context("reading query")?;
    if query.trim().is_empty() {
        return Ok(());
    }
    println!("{}", style("Searching…").dim());
    let hits = skill_registry::search(query.trim(), 20).await?;
    if hits.is_empty() {
        println!(
            "{}",
            style(format!("no skills match '{}'", query.trim())).dim()
        );
        return Ok(());
    }
    let mut items: Vec<String> = hits
        .iter()
        .map(|s| {
            format!(
                "{}  {}",
                s.id(),
                style(s.summary_line().splitn(2, " — ").nth(1).unwrap_or("")).dim()
            )
        })
        .collect();
    items.push("Cancel".to_string());
    let pick = match Select::with_theme(&theme)
        .with_prompt("Install which skill?")
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) if i < hits.len() => i,
        _ => return Ok(()),
    };
    let chosen = &hits[pick];
    let sk = skill_registry::install(&chosen.id()).await?;
    println!(
        "{} '{}'.",
        style("✓ installed").color256(splash::ACCENT),
        sk.name
    );
    Ok(())
}

/// Author a new persona interactively (name + role + voice + multi-line description).
fn persona_new_interactive() -> Result<()> {
    let theme = ui_theme();
    let name: String = Input::with_theme(&theme)
        .with_prompt("Persona name (e.g. Aria)")
        .interact_text()?;
    if name.trim().is_empty() {
        anyhow::bail!("a persona name is required");
    }
    let role: String = Input::with_theme(&theme)
        .with_prompt("Role (one line, e.g. a sharp senior-engineer mentor)")
        .allow_empty(true)
        .interact_text()?;
    let voice: String = Input::with_theme(&theme)
        .with_prompt("Voice (e.g. concise, warm, a little sardonic)")
        .allow_empty(true)
        .interact_text()?;
    println!(
        "{}",
        style("Backstory / values / how it behaves — end with a line containing only '.'").dim()
    );
    let body = read_multiline_until_dot()?;
    if body.trim().is_empty() {
        anyhow::bail!("a description is required");
    }
    let path = persona::save(&name, &role, &voice, &body)?;
    println!(
        "{}",
        style(format!("saved persona → {}", path.display())).color256(splash::ACCENT)
    );
    Ok(())
}

/// Paste a raw character / system prompt and have the model distill it into a persona card
/// (name + role + voice + a rewritten body). Then offer to activate it for the current chat.
async fn persona_paste_interactive(history: &mut Vec<Message>, model: &str) -> Result<()> {
    let theme = ui_theme();
    println!(
        "{}",
        style("Paste the character / system prompt below — end with a line containing only '.'")
            .dim()
    );
    let pasted = read_multiline_until_dot()?;
    if pasted.trim().is_empty() {
        anyhow::bail!("nothing pasted");
    }
    let (base_url, api_key, model_id) = resolve_endpoint(None, None, None)
        .context("need an endpoint to auto-create — run /config first")?;
    let http = http_client()?;
    let sys = Message::system(
        "You convert a pasted character / system prompt into a structured persona card. Extract a \
         short NAME (a proper name if one is given, else invent a fitting one), a one-line ROLE, a \
         short comma-separated VOICE (tone/style), and rewrite the remainder into a concise \
         second-person character BODY (backstory, values, behavior, boundaries). Keep the body \
         faithful to the source; do not invent unrelated facts. Reply with ONLY a JSON object: \
         {\"name\":\"\",\"role\":\"\",\"voice\":\"\",\"body\":\"\"}.",
    );
    let usr = Message::user(format!("Pasted character prompt:\n{pasted}"));
    println!("{}", style("distilling into a persona card…").dim());
    let resp = chore_chat(&http, &base_url, &api_key, &model_id, &[sys, usr], &[])
        .await
        .context("model call failed")?;
    let content = resp.content.unwrap_or_default();
    let json = extract_json_object(&content).context("model did not return a persona card")?;
    let v: serde_json::Value = serde_json::from_str(json).context("parsing the persona card")?;
    let name = v
        .get("name")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let role = v
        .get("role")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let voice = v
        .get("voice")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let body = v
        .get("body")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let name = if name.is_empty() {
        "Character".to_string()
    } else {
        name
    };
    let body = if body.is_empty() {
        pasted.trim().to_string()
    } else {
        body
    };

    let path = persona::save(&name, &role, &voice, &body)?;
    println!(
        "{}",
        style(format!("created persona '{name}' → {}", path.display())).color256(splash::ACCENT)
    );
    println!(
        "  {} {}",
        style("role:").dim(),
        if role.is_empty() {
            "(none)".into()
        } else {
            role
        }
    );
    println!(
        "  {} {}",
        style("voice:").dim(),
        if voice.is_empty() {
            "(none)".into()
        } else {
            voice
        }
    );

    if Confirm::with_theme(&theme)
        .with_prompt(format!("Play as {name} now?"))
        .default(true)
        .interact()?
    {
        let mut cfg = cli_config::load();
        cfg.persona = Some(name.clone());
        cli_config::save(&cfg)?;
        update_system_prompt(history, model);
        println!(
            "{}",
            style(format!("now playing: {name}")).color256(splash::ACCENT)
        );
    }
    Ok(())
}

/// Show a character's accumulated self-memory (reflected insights + recent episodes).
fn persona_self_view(slug: &str, name: &str) {
    persona_self_view_n(slug, name, false);
}

/// As [`persona_self_view`]; `all` lifts the per-section head cut.
///
/// The head cut exists so a glance stays a glance, but at a saturated cap this view is ALSO the
/// only place a self-memory id is printed, and `persona forget <id>` is the only way to make room.
/// Showing 10 of 40 while advising "retire one" offers a choice out of a set the reader cannot see —
/// so the hidden count is always stated, and `--all` prints the rest.
pub(crate) fn persona_self_view_n(slug: &str, name: &str, all: bool) {
    let mut mems = persona::self_mem::list(slug);
    if mems.is_empty() {
        println!(
            "{}",
            style(format!(
                "{name} has no self-memory yet — it grows as you chat."
            ))
            .dim()
        );
        return;
    }
    let (eps, ins) = persona::self_mem::counts(slug);
    println!(
        "{}",
        style(format!("{name} — {ins} insight(s), {eps} episode(s)"))
            .color256(splash::ACCENT)
            .bold()
    );
    if persona::self_mem::should_reflect(slug) {
        println!(
            "{}",
            style("  → primed to reflect: the next turn synthesizes recent episodes into insights")
                .dim()
        );
    }
    // insights first (the durable layer), newest first
    mems.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    let insights: Vec<&persona::self_mem::SelfMemory> = mems
        .iter()
        .filter(|m| m.kind == persona::self_mem::Kind::Insight)
        .collect();
    if !insights.is_empty() {
        println!("\n{}", style("insights").dim());
        let shown = if all { insights.len() } else { 10 };
        for m in insights.iter().take(shown) {
            println!(
                "  {} [{}] {}",
                style("★").color256(splash::ACCENT),
                m.importance,
                elide(m.body.trim(), 140)
            );
            // The id is what `persona forget <id>` names. Without it printed here the retire path is
            // unreachable in practice — ids are body-derived slugs nobody can guess.
            println!("      {}", style(&m.id).dim());
        }
        if insights.len() > shown {
            println!(
                "  {}",
                style(format!(
                    "… {} more — `persona self {name} --all` lists every id",
                    insights.len() - shown
                ))
                .dim()
            );
        }
    }
    let episodes: Vec<&persona::self_mem::SelfMemory> = mems
        .iter()
        .filter(|m| m.kind == persona::self_mem::Kind::Episode)
        .collect();
    if !episodes.is_empty() {
        println!("\n{}", style("recent episodes").dim());
        let shown = if all { episodes.len() } else { 8 };
        for m in episodes.iter().take(shown) {
            println!(
                "  {} [{}] {}",
                style("·").dim(),
                m.importance,
                elide(m.body.trim(), 120)
            );
        }
        if episodes.len() > shown {
            println!(
                "  {}",
                style(format!("… {} more", episodes.len() - shown)).dim()
            );
        }
    }
    let (_, ins_n) = persona::self_mem::counts(slug);
    if ins_n >= persona::self_mem::INSIGHT_CAP {
        // At a saturated cap the eviction order can no longer make room: a wrong-but-important
        // insight outranks it indefinitely, so say so and name the verb that fixes it.
        println!(
            "\n{}",
            style(format!(
                "insights are at the {} cap — `aizen persona forget <id>` retires one (archived)",
                persona::self_mem::INSIGHT_CAP
            ))
            .dim()
        );
    }
    let arch = persona::self_mem::list_archive(slug);
    if !arch.is_empty() {
        println!(
            "{}",
            style(format!(
                "{} retired — `aizen persona unforget <id>` brings one back",
                arch.len()
            ))
            .dim()
        );
    }
}

/// `/persona` — pick the character the agent role-plays (or author / paste / clear one), and manage
/// its evolving self-memory. The active persona is injected as `<persona>` (+ `<self>`) in the
/// system prompt; switching applies to the current chat in place.
pub(crate) async fn personas_menu(history: &mut Vec<Message>, model: &str) -> Result<()> {
    loop {
        let theme = ui_theme();
        let personas = persona::list();
        let active = cli_config::load().persona;
        let active_slug = active.as_deref().map(skill::sanitize_name);
        let n = personas.len();
        let mut items: Vec<String> = personas
            .iter()
            .map(|p| {
                let on = active_slug.as_deref() == Some(skill::sanitize_name(&p.name).as_str());
                let badge = if on {
                    style("●").color256(splash::ACCENT).to_string()
                } else {
                    style("○").dim().to_string()
                };
                let sub = if p.role.is_empty() {
                    p.voice.clone()
                } else {
                    p.role.clone()
                };
                format!(
                    "{badge}  {}{}  —  {}",
                    icons::g(icons::slash("persona")),
                    p.name,
                    style(sub).dim()
                )
            })
            .collect();
        // actions after the persona list
        let active_slug_self = active_slug.clone();
        let (n_eps, n_ins) = active_slug_self
            .as_deref()
            .map(persona::self_mem::counts)
            .unwrap_or((0, 0));
        let has_self = n_eps + n_ins > 0;
        let mut actions: Vec<String> = vec![
            "+ New persona".to_string(),
            "Paste a character prompt → auto-create".to_string(),
        ];
        if active.is_some() {
            actions.push(format!(
                "Evolution: {} (toggle)",
                if persona_evolve_enabled() {
                    "ON"
                } else {
                    "OFF"
                }
            ));
        }
        if has_self {
            actions.push(format!(
                "View self-memory ({n_ins} insights, {n_eps} episodes)"
            ));
            actions.push("Reset self-memory".to_string());
        }
        if active.is_some() {
            actions.push("Use default voice (no persona)".to_string());
        }
        if n > 0 {
            actions.push("Delete a persona".to_string());
        }
        actions.push("Back".to_string());
        items.extend(actions.iter().cloned());

        let prompt = format!(
            "Persona — active: {} (Esc to go back)",
            active.as_deref().unwrap_or("(default)")
        );
        let pick = match Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(&items)
            .default(0)
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            // select this persona
            let mut cfg = cli_config::load();
            cfg.persona = Some(personas[pick].name.clone());
            cli_config::save(&cfg)?;
            update_system_prompt(history, model);
            println!(
                "{}",
                style(format!("now playing: {}", personas[pick].name)).color256(splash::ACCENT)
            );
            return Ok(());
        }
        match actions[pick - n].as_str() {
            "+ New persona" => {
                if let Err(e) = persona_new_interactive() {
                    tui::note_line(&format!("{} {e}", style("persona:").red()));
                }
            }
            "Paste a character prompt → auto-create" => {
                if let Err(e) = persona_paste_interactive(history, model).await {
                    tui::note_line(&format!("{} {e}", style("persona:").red()));
                }
            }
            a if a.starts_with("Evolution:") => {
                let mut cfg = cli_config::load();
                let now = !persona_evolve_enabled();
                cfg.persona_evolve = Some(now);
                cli_config::save(&cfg)?;
                println!(
                    "{}",
                    style(format!(
                        "persona evolution {}",
                        if now { "ON" } else { "OFF" }
                    ))
                    .color256(splash::ACCENT)
                );
            }
            a if a.starts_with("View self-memory") => {
                if let Some(slug) = active_slug.as_deref() {
                    let name = active.as_deref().unwrap_or(slug);
                    persona_self_view(slug, name);
                }
            }
            "Reset self-memory" => {
                if let Some(slug) = active_slug.as_deref() {
                    let n = persona::self_mem::reset(slug);
                    update_system_prompt(history, model);
                    println!(
                        "{}",
                        style(format!("reset self-memory ({n} item(s) cleared)"))
                            .color256(splash::ACCENT)
                    );
                }
            }
            "Use default voice (no persona)" => {
                let mut cfg = cli_config::load();
                cfg.persona = None;
                cli_config::save(&cfg)?;
                update_system_prompt(history, model);
                println!(
                    "{}",
                    style("persona cleared → default assistant voice").color256(splash::ACCENT)
                );
                return Ok(());
            }
            "Delete a persona" => {
                let names: Vec<String> = personas.iter().map(|p| p.name.clone()).collect();
                if let Ok(Some(i)) = Select::with_theme(&theme)
                    .with_prompt(
                        "Retire which persona? (card + self-memory archived — Esc to cancel)",
                    )
                    .items(&names)
                    .default(0)
                    .interact_opt()
                {
                    match persona::delete(&names[i]) {
                        Ok(true) => {
                            // A retired character must not stay named as the active one.
                            let mut cfg = cli_config::load();
                            if cfg
                                .persona
                                .as_deref()
                                .map(|p| skill::sanitize_name(p) == skill::sanitize_name(&names[i]))
                                .unwrap_or(false)
                            {
                                cfg.persona = None;
                                cli_config::save(&cfg)?;
                                update_system_prompt(history, model);
                            }
                            println!(
                                "{}",
                                style(format!(
                                    "retired '{}' — `aizen persona restore {}` brings it back",
                                    names[i], names[i]
                                ))
                                .color256(splash::ACCENT)
                            );
                        }
                        Ok(false) => println!("{}", style("(already gone)").dim()),
                        Err(e) => tui::note_line(&format!("{} {e}", style("persona:").red())),
                    }
                }
            }
            _ => return Ok(()), // Back
        }
    }
}

/// `/telegram` — a dedicated sub-menu for the Telegram integration (one of Aizen's connected
/// apps): set up, test, status, start the phone-control daemon, or disable.
pub(crate) async fn telegram_menu() -> Result<()> {
    let theme = ui_theme();
    let configured = telegram::is_configured();
    let status = if configured {
        "configured"
    } else {
        "not set up"
    };
    let items = [
        "Set up / reconfigure  (paste @BotFather token, capture chat id)",
        "Send a test message",
        "Status",
        "Start daemon  (control aizen from your phone — Ctrl-C to stop)",
        "Disable  (remove the bot config)",
        "Back",
    ];
    let pick = match Select::with_theme(&theme)
        .with_prompt(format!("Telegram — {status} (Esc to go back)"))
        .items(&items)
        .default(if configured { 2 } else { 0 })
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match pick {
        0 => telegram_setup().await,
        1 => telegram_test().await,
        2 => telegram_status().await,
        3 => hostbot::run_serve(Vec::new()).await, // interactive: host every bot this machine may run
        4 => telegram_disable(),
        _ => Ok(()),
    }
}

/// Interactive Telegram setup: paste the @BotFather token, validate via getMe, then capture the
/// owner's chat id from the first message they send the bot.
async fn telegram_setup() -> Result<()> {
    let theme = ui_theme();
    println!(
        "\n{}",
        style("Telegram setup").bold().color256(splash::ACCENT)
    );
    println!(
        "{}",
        style("Create a bot with @BotFather (/newbot), copy the token it gives you.").dim()
    );

    let mut cfg = cli_config::load();
    let mut tg = cfg.telegram.clone().unwrap_or_default();
    let cur = tg
        .token
        .as_deref()
        .map(cli_config::mask)
        .unwrap_or_else(|| "none".to_string());
    let entered = Password::with_theme(&theme)
        .with_prompt(format!("Bot token (current {cur} — Enter to keep)"))
        .allow_empty_password(true)
        .interact()
        .context("reading token")?;
    if !entered.trim().is_empty() {
        tg.token = Some(entered.trim().to_string());
    }
    let token = tg.token.clone().context("a bot token is required")?;

    let client = telegram::Client::new(token)?;
    let username = client
        .get_me()
        .await
        .context("Telegram rejected the token — check it and retry")?;
    println!(
        "{}",
        style(format!("✓ bot @{username}")).color256(splash::ACCENT)
    );

    println!(
        "{}",
        style(format!(
            "Now open Telegram → find @{username} → send it any message. Waiting (≤120s)…"
        ))
        .dim()
    );
    let chat = poll_for_chat_id(&client).await?;
    if !tg.allowed_chat_ids.contains(&chat) {
        tg.allowed_chat_ids.push(chat);
    }
    println!(
        "{}",
        style(format!("✓ captured chat id {chat}")).color256(splash::ACCENT)
    );

    cfg.telegram = Some(tg);
    cli_config::save(&cfg)?;
    let _ = client
        .send_message(
            chat,
            "✅ Aizen connected. Run `aizen serve`, then send /help.",
        )
        .await;
    println!(
        "\n{}",
        style("Saved. Start the daemon with:  aizen serve").color256(splash::ACCENT)
    );
    Ok(())
}

/// Long-poll until the owner sends the bot a message; return that chat id (≤120s, else error).
async fn poll_for_chat_id(client: &telegram::Client) -> Result<i64> {
    let start = tokio::time::Instant::now();
    let mut offset = 0i64;
    while start.elapsed() < std::time::Duration::from_secs(120) {
        let updates = client
            .get_updates(offset, 20)
            .await
            .context("polling for your message")?;
        for u in &updates {
            offset = offset.max(u.update_id + 1);
        }
        for u in updates {
            if let Some(msg) = u.message {
                return Ok(msg.chat.id);
            }
        }
    }
    anyhow::bail!("timed out waiting for a message — run `aizen telegram setup` again")
}
