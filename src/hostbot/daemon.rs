//! The generic host-bot daemon — ONE serial agent runner that drives any `Platform`.
//!
//! `aizen serve` (Telegram) and `aizen discord serve` (Discord) both funnel into `run_daemon<P>`:
//! the platform spawns its listener(s), inbound messages arrive on one channel tagged with the
//! originating sub-bot (`Inbound::route`), and a single loop processes them one at a time — chatting,
//! running `/commands`, and (on platforms that support it) routing a destructive-op approval back to
//! the SAME bot the request came from. Because it's serial, that per-turn approval route never races.
//!
//! Sessions are keyed by `(route, chat)` and persisted to `~/.aizen/hostbot/sessions/` after every
//! turn, so a daemon restart (the point of `Restart=always` self-host) keeps its context.

use anyhow::{Context, Result};
use console::style;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, Sender};

use crate::agent::{self, AgentConfig, StopReason};
use crate::core::approval::ApprovalMode;
use crate::core::types::Message;
use crate::hostbot::platform::{Inbound, Platform};
use crate::hostbot::platforms;
use crate::hostbot::store;
use crate::llm::client;

/// The single source of truth for the command surface: `(name, arg-hint, description)`. Published to
/// Telegram's "/" menu via `setMyCommands` and rendered into `/help`. The dispatcher arms in
/// `handle_command` must stay in sync with this list.
const SERVE_COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "list commands"),
    ("new", "", "start a fresh conversation (drop context)"),
    ("resume", "", "how much context is kept"),
    ("sh", "<cmd>", "run a shell command now (dangerous-command floor still applies)"),
    ("agent", "<task>", "run the agent autonomously (file edits / shell, no per-op ask)"),
    ("cd", "<path>", "change working directory (affects /sh and the agent)"),
    ("pwd", "", "show working directory"),
    ("approval", "<ask|smart|yolo>", "set agent approval level"),
    ("ultimate", "", "toggle ultimate mode (max effort + prefer workflows)"),
    ("effort", "<level>", "reasoning effort: auto | low | medium | high | xhigh | max | off"),
    ("model", "[name]", "show or switch the model"),
    ("memory", "[query]", "show profile / search; \"memory remember <fact>\" to save"),
    ("tools", "", "list tool bundles"),
    ("status", "", "cwd · model · approval mode · effort · context size"),
    ("bots", "", "list the bots this daemon hosts"),
    ("addbot", "<name> <token>", "host another bot from this daemon (Telegram only)"),
    ("rmbot", "<name>", "stop hosting a bot (cannot remove \"default\")"),
];

/// Build the `/help` reply from `SERVE_COMMANDS` so it always matches the app's "/" menu.
fn serve_help_text() -> String {
    let mut s = String::from("Aizen bot — control this machine from chat. Commands:\n");
    for (name, hint, desc) in SERVE_COMMANDS {
        if hint.is_empty() {
            s.push_str(&format!("/{name} — {desc}\n"));
        } else {
            s.push_str(&format!("/{name} {hint} — {desc}\n"));
        }
    }
    s.push_str(
        "\nAny other message → the agent (chats + uses tools). Default: destructive ops ask for \
         approval here (✓/✗ where supported). `/approval smart` auto-runs read-only shell; \
         `/approval yolo` pre-authorizes the rest after the hard floor.",
    );
    s
}

/// The command menu as `(name, description)` pairs — what a platform publishes to its slash menu.
fn command_menu() -> Vec<(String, String)> {
    SERVE_COMMANDS.iter().map(|(n, _, d)| (n.to_string(), d.to_string())).collect()
}

/// Split a string so each chunk is `<= max` **UTF-16 code units** — the unit Telegram (4096) and
/// Discord (2000) actually count their message caps in. Splitting by Unicode scalar undercounts:
/// an astral char (emoji, math-bold) is 1 scalar but 2 UTF-16 units, so an emoji-heavy reply under
/// the char cap can still exceed the platform limit → HTTP 400 → the reply is silently dropped.
fn chunk_text(s: &str, max: usize) -> Vec<String> {
    if s.encode_utf16().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_units = 0usize;
    for ch in s.chars() {
        let u = ch.len_utf16();
        if cur_units + u > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_units = 0;
        }
        cur.push(ch);
        cur_units += u;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Hard cap on messages retained in one serve session, so a long conversation can't grow without
/// bound. Generous — the mid-loop context guard handles within-turn pressure.
const SERVE_SESSION_MAX_MSGS: usize = 40;

/// Bound a serve session's history: drop the OLDEST whole turns (keeping the system prompt at [0])
/// until under `max`, always cutting at a `user` boundary so an assistant tool-call turn is never
/// split from its tool results (a dangling tool_call ⇒ a 400 on strict gateways).
fn cap_session(history: &mut Vec<Message>, max: usize) {
    while history.len() > max {
        let second_user =
            history.iter().enumerate().filter(|(i, m)| *i >= 1 && m.role == "user").nth(1).map(|(i, _)| i);
        match second_user {
            Some(i) => {
                history.drain(1..i);
            }
            None => break,
        }
    }
}

/// Run one serve turn over a PERSISTENT per-chat history, so follow-ups like "now fix it" keep
/// context. Seeds the system prompt (with memory + SOUL + persona) once per session, appends the
/// user task, drives the loop, learns passively, and bounds the history. A `clarify` yield leaves a
/// resumable history (the owner's next message is the answer).
async fn run_serve_turn(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    history: &mut Vec<Message>,
    task: &str,
    approval_mode: ApprovalMode,
) -> Result<String> {
    if history.is_empty() {
        history.push(Message::system(crate::current_system_prompt(model)));
    }
    history.push(Message::user(task.to_string()));

    // Default-ON lazy LSP (same as REPL/agent CLI) so remote coding turns get symbol tools.
    crate::agent::lsp::LSP.set_request_timeout(AgentConfig::default().lsp_request_timeout_secs);
    let _ = crate::agent::lsp::LSP.enable();
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        crate::resolve_ctx_window(model).0,
    )?;
    let cfg = AgentConfig {
        approval_mode,
        quiet: true,
        enable_verify_gate: false,
        context_window: crate::resolve_ctx_window(model).0,
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        ..Default::default()
    };
    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    let chat = move |msgs: Vec<Message>, defs: Vec<crate::core::types::ToolDef>| async move {
        client::chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
    };
    let sum_ep = crate::summarizer_endpoint(base, key, model_ref);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    let outcome = agent::run_agent_loop_compacting(chat, summarize, &cfg, &registry, history).await?;

    crate::maybe_learn_memory(history);
    cap_session(history, SERVE_SESSION_MAX_MSGS);

    if let StopReason::AwaitingInput(q) = &outcome.stop {
        return Ok(format!("❓ {q}"));
    }
    Ok(outcome
        .final_text
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(the agent produced no answer)".to_string()))
}

// ── the generic daemon loop ──────────────────────────────────────────────────────────

/// Drive one `Platform` to completion: spawn its listeners, then serially handle inbound messages —
/// `/commands` via `handle_command`, everything else as an agent turn (with per-turn approval routing
/// where the platform supports it). Sessions are loaded from disk at start and saved after each turn.
async fn run_daemon<P: Platform>(platform: P) -> Result<()> {
    let platform = std::sync::Arc::new(platform);
    let (base_url, api_key, mut model) = crate::resolve_endpoint(None, None, None)
        .context("configure the model endpoint first (run `aizen config`)")?;
    let http = crate::http_client()?;

    let (tx, mut rx) = mpsc::channel::<Inbound<P::Chat>>(64);
    platform.start(tx.clone()).await?;

    // Restore persisted conversations for THIS platform (parse the chat id back via `Chat::from_str`).
    let mut sessions: HashMap<(String, P::Chat), Vec<Message>> = HashMap::new();
    for (route, chat_s, msgs) in store::load_sessions(platform.name()) {
        if let Ok(chat) = chat_s.parse::<P::Chat>() {
            sessions.insert((route, chat), msgs);
        }
    }

    eprintln!(
        "{}",
        style(format!("aizen serve — {} live (Ctrl-C to stop)", platform.name())).dim()
    );

    loop {
        let inbound = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => { eprintln!("\nshutting down…"); crate::agent::process::kill_all(); break; }
            m = rx.recv() => match m { Some(m) => m, None => break },
        };
        let Inbound { route, chat, text } = inbound;
        let trimmed = text.trim().to_string();

        // A `/command` → the dispatcher. `Some(reply)` = handled; `None` = fall through to the agent.
        if trimmed.starts_with('/') {
            let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
            let name = parts.next().unwrap_or("").trim().to_string();
            let arg = parts.next().unwrap_or("").trim().to_string();
            if let Some(reply) =
                handle_command(&*platform, &name, &arg, &mut sessions, &route, chat, &mut model, &tx).await
            {
                for piece in chunk_text(&reply, platform.message_max()) {
                    let _ = platform.send(&route, chat, &piece).await;
                }
                continue;
            }
        }

        // `/agent <task>` is explicitly autonomous; a plain message follows the persisted approval tier.
        let (task, approval) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), ApprovalMode::Yolo),
            None => (trimmed.clone(), crate::approval_mode()),
        };
        if task.is_empty() {
            continue;
        }
        let _ = platform.send(&route, chat, "⏳ working…").await;
        // Pin the approval route to THIS bot+chat so a destructive-op prompt returns here (serial loop
        // ⇒ no race). No-op on platforms without inline approval (Discord) — those auto-deny + skip.
        if platform.supports_approval() {
            platform.set_approval_route(&route, chat);
        }
        // Pin this sub-bot's persona for the turn (serial loop ⇒ no race). Only the `<persona>`/`<self>`
        // blocks change — `<user_memory>` stays global, so memory is always the primary agent's. `None`
        // (the "default" bot / a bot with no persona) falls back to the global `config.persona`.
        crate::persona::set_override(platform.persona_for(&route));
        let history = sessions.entry((route.clone(), chat)).or_default();
        let reply = run_serve_turn(&http, &base_url, &api_key, &model, history, &task, approval)
            .await
            .unwrap_or_else(|e| format!("error: {e}"));
        crate::persona::set_override(None);
        if platform.supports_approval() {
            platform.clear_approval_route();
        }
        // Persist the updated history so a restart keeps this chat's context.
        let _ = store::save_session(platform.name(), &route, &chat.to_string(), history);
        for piece in chunk_text(&reply, platform.message_max()) {
            let _ = platform.send(&route, chat, &piece).await;
        }
    }

    platform.shutdown();
    Ok(())
}

/// Dispatch a `/command`. Returns `Some(reply)` when we own it, or `None` to fall through to the
/// agent path (`/agent`, or an unknown `/foo`). Config toggles persist to `CliConfig` so the next
/// turn's LLM request picks them up. Multi-bot arms (`/bots /addbot /rmbot`) delegate to the platform
/// and are gated on `supports_multibot` (Discord replies "Telegram only").
#[allow(clippy::too_many_arguments)]
async fn handle_command<P: Platform>(
    platform: &P,
    name: &str,
    arg: &str,
    sessions: &mut HashMap<(String, P::Chat), Vec<Message>>,
    route: &str,
    chat: P::Chat,
    model: &mut String,
    tx: &Sender<Inbound<P::Chat>>,
) -> Option<String> {
    use crate::core::cli_config;
    let key = (route.to_string(), chat);
    match name {
        "help" | "start" => Some(serve_help_text()),
        "new" | "reset" => {
            sessions.remove(&key);
            store::drop_session(platform.name(), route, &chat.to_string());
            Some("🆕 started a fresh conversation — earlier context dropped.".to_string())
        }
        "resume" => {
            let turns = sessions.get(&key).map(|h| h.iter().filter(|m| m.role == "user").count()).unwrap_or(0);
            Some(if turns == 0 {
                "🧵 no active conversation — just send a message to start one.".to_string()
            } else {
                format!("🧵 continuing — {turns} message(s) of context kept. /new to start over.")
            })
        }
        // `/sh <cmd>` — run NOW (like the REPL `!cmd` escape). The hard safety floor still refuses
        // catastrophic commands; everything else runs without a per-op approval prompt.
        "sh" | "shell" | "run" => {
            let cmd = arg.trim();
            if cmd.is_empty() {
                return Some("usage: /sh <command>".to_string());
            }
            match crate::agent::cmd_guard::classify(cmd) {
                crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                    Some(format!("✗ blocked by the safety floor: {reason}"))
                }
                _ => {
                    let c = cmd.to_string();
                    let out = tokio::task::spawn_blocking(move || crate::run_shell_escape(&c))
                        .await
                        .unwrap_or_else(|e| format!("[task failed: {e}]"));
                    Some(format!("$ {cmd}\n{out}"))
                }
            }
        }
        // `/cd <path>` + `/pwd` — the daemon is one process, so set_current_dir affects BOTH /sh and
        // the agent (its system prompt embeds cwd, rebuilt next turn).
        "cd" => {
            let path = arg.trim();
            if path.is_empty() {
                return Some("usage: /cd <path>".to_string());
            }
            let p = std::path::Path::new(path);
            if !p.exists() {
                return Some(format!("✗ no such path: {path}"));
            }
            if !p.is_dir() {
                return Some(format!("✗ not a directory: {path}"));
            }
            match std::env::set_current_dir(p) {
                Ok(()) => {
                    let cwd = std::env::current_dir().map(|c| c.display().to_string()).unwrap_or_else(|_| path.to_string());
                    Some(format!("📁 cwd → {cwd}"))
                }
                Err(e) => Some(format!("✗ cd failed: {e}")),
            }
        }
        "pwd" => {
            let cwd = std::env::current_dir().map(|c| c.display().to_string()).unwrap_or_else(|_| "?".to_string());
            Some(format!("📁 {cwd}"))
        }
        "approval" => {
            let requested = arg.split_whitespace().next().unwrap_or("status");
            let mut cfg = cli_config::load();
            let saved = cfg.persisted_approval_mode();
            if requested.is_empty() || matches!(requested, "status" | "st") {
                return Some(format!(
                    "approval: {} (saved: {saved}) · ask=prompt · smart=read-only auto · yolo=pre-authorized",
                    cli_config::approval_mode()
                ));
            }
            let mode = match requested.parse::<ApprovalMode>() {
                Ok(mode) => mode,
                Err(e) => return Some(format!("usage: /approval ask|smart|yolo ({e})")),
            };
            cfg.set_approval_mode(mode);
            match cli_config::save(&cfg) {
                Ok(_) => Some(format!("approval → {mode}")),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "yolo" | "auto" | "yes" => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Yolo {
                ApprovalMode::Ask
            } else {
                ApprovalMode::Yolo
            };
            cfg.set_approval_mode(mode);
            match cli_config::save(&cfg) {
                Ok(_) => Some(format!("approval → {mode} (legacy /yolo alias)")),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "smart" => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Smart {
                ApprovalMode::Ask
            } else {
                ApprovalMode::Smart
            };
            cfg.set_approval_mode(mode);
            match cli_config::save(&cfg) {
                Ok(_) => Some(format!("approval → {mode} (legacy /smart alias)")),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "ultimate" | "ultra" => {
            let mut cfg = cli_config::load();
            let now = !cfg.ultimate.unwrap_or(false);
            cfg.ultimate = Some(now);
            if now {
                cfg.reasoning_effort = Some("max".to_string());
                cfg.auto_effort = Some(false);
            } else {
                cfg.reasoning_effort = None;
                cfg.auto_effort = None;
            }
            match cli_config::save(&cfg) {
                Ok(_) if now => Some("✦ ultimate ON — max reasoning effort every turn + prefers launching workflows. /ultimate again to turn it off.".to_string()),
                Ok(_) => Some("ultimate OFF — effort back to auto-detect.".to_string()),
                Err(e) => Some(format!("ultimate: {e}")),
            }
        }
        "effort" => {
            let sub = arg.trim().to_ascii_lowercase();
            let mut cfg = cli_config::load();
            match sub.as_str() {
                "" | "status" | "st" => {
                    let e = cfg.reasoning_effort.as_deref().unwrap_or("(unset)");
                    let auto = if cfg.auto_effort == Some(false) { "off" } else { "on" };
                    Some(format!("effort: {e} · auto {auto}"))
                }
                "auto" | "on" => {
                    cfg.auto_effort = None;
                    match cli_config::save(&cfg) {
                        Ok(_) => Some("effort auto ON — detected from each message.".to_string()),
                        Err(e) => Some(format!("effort: {e}")),
                    }
                }
                "off" => {
                    cfg.auto_effort = Some(false);
                    match cli_config::save(&cfg) {
                        Ok(_) => Some("effort auto OFF — uses the fixed reasoning_effort (or omits it).".to_string()),
                        Err(e) => Some(format!("effort: {e}")),
                    }
                }
                "low" | "medium" | "high" | "xhigh" | "max" => {
                    cfg.reasoning_effort = Some(sub.clone());
                    cfg.auto_effort = Some(false);
                    match cli_config::save(&cfg) {
                        Ok(_) => Some(format!("effort pinned to {sub} (auto off).")),
                        Err(e) => Some(format!("effort: {e}")),
                    }
                }
                "none" | "clear" => {
                    cfg.reasoning_effort = None;
                    cfg.auto_effort = None;
                    match cli_config::save(&cfg) {
                        Ok(_) => Some("effort cleared — auto ON, no fixed tier.".to_string()),
                        Err(e) => Some(format!("effort: {e}")),
                    }
                }
                other => Some(format!("usage: /effort auto|off|low|medium|high|xhigh|max|clear  (unknown '{other}')")),
            }
        }
        "model" => {
            let want = arg.trim();
            if want.is_empty() {
                return Some(format!("model: {model}"));
            }
            *model = want.to_string();
            let mut cfg = cli_config::load();
            cfg.model = Some(want.to_string());
            let saved = cli_config::save(&cfg);
            sessions.remove(&key); // next turn rebuilds the system prompt with the new model/ctx-window
            store::drop_session(platform.name(), route, &chat.to_string());
            match saved {
                Ok(_) => Some(format!("🔄 model → {want} (context reset for this chat)")),
                Err(e) => Some(format!("model: {e}")),
            }
        }
        "memory" | "mem" => {
            if let Some(rest) = arg.strip_prefix("remember").map(str::trim).filter(|s| !s.is_empty()) {
                Some(match crate::memory::remember(rest) {
                    Ok(id) => format!("🧠 remembered ({id})"),
                    Err(e) => format!("memory: {e}"),
                })
            } else if arg.trim().is_empty() {
                Some("usage: /memory <query>  ·  /memory remember <fact>".to_string())
            } else {
                Some(match crate::memory::search_scoped(arg.trim(), 5, &crate::memory::ScopeSel::default_view()) {
                    Ok(hits) if hits.is_empty() => format!("(no memory matches '{}')", arg.trim()),
                    Ok(hits) => {
                        let mut s = String::new();
                        for h in &hits {
                            let body: String = h.entry.body.chars().take(160).collect();
                            s.push_str(&format!("• {} — {}\n", h.entry.name, body.replace('\n', " ")));
                        }
                        s.trim_end().to_string()
                    }
                    Err(e) => format!("memory: {e}"),
                })
            }
        }
        "tools" | "toolsets" => Some(crate::agent::toolsets::format_config_status()),
        "status" => {
            let cfg = cli_config::load();
            let cwd = std::env::current_dir().map(|c| c.display().to_string()).unwrap_or_else(|_| "?".to_string());
            let approval = cli_config::approval_mode();
            let effort = cfg.reasoning_effort.as_deref().unwrap_or("auto");
            let ultimate = if cfg.ultimate.unwrap_or(false) { "on" } else { "off" };
            let turns = sessions.get(&key).map(|h| h.iter().filter(|m| m.role == "user").count()).unwrap_or(0);
            Some(format!(
                "📊 aizen serve\nplatform: {}\nbot:      {route}\ncwd:      {cwd}\nmodel:    {model}\napproval: {approval}\neffort:   {effort} · ultimate {ultimate}\ncontext:  {turns} message(s) this chat",
                platform.name()
            ))
        }
        // ── multi-bot management: hot add/remove extra bots (Telegram only) ──
        "bots" => {
            let mut bots = platform.list_bots();
            bots.sort_by(|a, b| a.name.cmp(&b.name));
            if bots.is_empty() {
                return Some(format!("🤖 {} — no hosted bots to list.", platform.name()));
            }
            let mut s = format!("🤖 {} bot(s) hosted:\n", bots.len());
            for b in bots {
                let tag = if b.name == "default" { " (primary)" } else { "" };
                s.push_str(&format!("• {}{tag} — @{} · {} chat(s)\n", b.name, b.username, b.chats));
            }
            if platform.supports_multibot() {
                s.push_str("\n/addbot <name> <token> to add · /rmbot <name> to remove");
            }
            Some(s.trim_end().to_string())
        }
        "addbot" => {
            if !platform.supports_multibot() {
                return Some("✗ hosting extra bots is only supported on Telegram.".to_string());
            }
            let mut it = arg.splitn(2, char::is_whitespace);
            let bot_name = it.next().unwrap_or("").trim().to_string();
            let token = it.next().unwrap_or("").trim().to_string();
            if bot_name.is_empty() || token.is_empty() {
                return Some("usage: /addbot <name> <token>".to_string());
            }
            match platform.add_bot(&bot_name, &token, tx).await {
                Ok(username) => Some(format!("✓ bot @{username} live as \"{bot_name}\" — open it in Telegram and send /start.")),
                Err(e) => Some(format!("✗ {e}")),
            }
        }
        "rmbot" => {
            if !platform.supports_multibot() {
                return Some("✗ hosting extra bots is only supported on Telegram.".to_string());
            }
            let bot_name = arg.trim();
            if bot_name.is_empty() {
                return Some("usage: /rmbot <name>".to_string());
            }
            match platform.remove_bot(bot_name).await {
                Ok(()) => {
                    sessions.retain(|(r, _), _| r != bot_name);
                    Some(format!("✓ stopped hosting \"{bot_name}\"."))
                }
                Err(e) => Some(format!("✗ {e}")),
            }
        }
        // `/agent` (autonomous run) and any unknown `/foo` → fall through to the agent path.
        _ => None,
    }
}

// ── platform entry points ──────────────────────────────────────────────────────────

/// `aizen serve` — host the Telegram platform (primary bot + any `/addbot`-hosted extras).
pub async fn run_serve() -> Result<()> {
    let platform = platforms::telegram::TelegramPlatform::from_config(command_menu())?;
    run_daemon(platform).await
}

/// `aizen discord serve` — host the Discord platform (one gateway, full command menu, no inline
/// approval so destructive ops are skipped).
pub async fn run_discord_serve() -> Result<()> {
    let platform = platforms::discord::DiscordPlatform::from_config()?;
    run_daemon(platform).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_commands_are_telegram_legal() {
        // Telegram requires command names 1–32 chars of [a-z0-9_]; the "/" menu registration fails
        // otherwise. Guard the whole table so a new command can't silently break the menu.
        assert!(!SERVE_COMMANDS.is_empty());
        for (name, _hint, desc) in SERVE_COMMANDS {
            assert!((1..=32).contains(&name.len()), "command '{name}' length out of range");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "command '{name}' has an illegal char"
            );
            assert!(!desc.is_empty(), "command '{name}' needs a description");
        }
        assert!(SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "addbot"), "/addbot must be in the menu");
        assert!(SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "rmbot"), "/rmbot must be in the menu");
        assert!(SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "bots"), "/bots must be in the menu");
    }

    #[test]
    fn help_text_lists_every_command() {
        let help = serve_help_text();
        for (name, _hint, _desc) in SERVE_COMMANDS {
            assert!(help.contains(&format!("/{name}")), "help missing /{name}");
        }
    }

    #[test]
    fn chunk_text_splits_on_utf16_units_not_scalars() {
        // 'A' = 1 UTF-16 unit; '𝐀' (math bold) = 2. A cap-3 chunk fits 3 A's or 1 math-bold + fill.
        let s = "𝐀".repeat(3000);
        let chunks = chunk_text(&s, 3500);
        for c in &chunks {
            assert!(c.encode_utf16().count() <= 3500, "chunk exceeds the UTF-16 cap");
        }
        assert!(chunks.len() > 1, "3000 astral chars = 6000 UTF-16 units must split");
    }

    #[test]
    fn chunk_text_keeps_short_ascii_whole() {
        assert_eq!(chunk_text("hello", 3500), vec!["hello".to_string()]);
    }

    #[test]
    fn cap_session_drops_oldest_whole_turns_at_user_boundary() {
        let mut h = vec![Message::system("sys")];
        for i in 0..5 {
            h.push(Message::user(format!("u{i}")));
            h.push(Message::assistant(format!("a{i}")));
        }
        cap_session(&mut h, 5);
        assert!(h.len() <= 5);
        assert_eq!(h[0].role, "system", "system prompt is preserved at [0]");
        assert_eq!(h[1].role, "user", "history still starts at a user boundary");
    }

    #[test]
    fn cap_session_keeps_single_turn_even_if_over_cap() {
        let mut h = vec![Message::system("sys"), Message::user("u0"), Message::assistant("a0")];
        cap_session(&mut h, 2);
        assert_eq!(h.len(), 3, "a single turn is never split even if over cap");
    }
}
