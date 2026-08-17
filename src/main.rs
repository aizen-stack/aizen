//! `aizen` — Aizen first-party agentic coding CLI.
//!
//! Subcommands:
//!   aizen chat    — OpenAI-compatible streaming chat (the v0 "call API like hermes" layer)
//!   aizen memory  — the standalone, best-for-CLI memory brain (see linked-riding-mochi.md)

// ─── module tree ────────────────────────────────────────────────────────────
// Domains that own a folder: the agent loop, the memory brain, personas, benches.
mod agent;
mod agents; // delegatable specialist sub-agent library (agency-agents format)
mod bench;
mod memory;
mod persona;
// Grouped by role (the src/ reorg — see each folder's mod.rs for what it holds):
mod channels; // notify + shared channel glue
mod core; // types · config · cli_config · approval · net_guard
mod features; // crawl · timemachine · cron · commands
mod hostbot; // generic Telegram/Discord daemon
mod llm; // the OpenAI-compatible chat client
mod skills; // skill store + registry
mod ui; // tui · theme · markdown · spinner · splash · icons · image_input

// Declaration-only: the command-line surface (clap derive types).
mod cli_args;

// The reorg moved 23 top-level files into the folders above. These re-exports keep the
// call sites in THIS file referring to the modules by their short names (no behavior
// change) — every other file already uses the new `crate::<group>::<mod>` paths.
use crate::agent::app_catalog;
use crate::channels::notify;
use crate::core::session_store::*;
use crate::core::{cli_config, config, types};
use crate::features::slash::{self, SlashId};
use crate::features::{commands, coop, crawl, cron, timemachine};
use crate::hostbot::platforms::{discord, telegram};
use crate::llm::client;
use crate::persona::soul;
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::{config_ui, icons, image_input, splash, theme, tui};

// The `clap` type tree (every subcommand enum) lives in its own file — see `cli_args`.
use cli_args::*;

use crate::core::approval::ApprovalMode;
use agent::{AgentConfig, AgentOutcome, StopReason};
use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use console::{style, Style};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use types::{Message, ToolDef};

/// Suppress Windows "hard error" dialogs process-wide (and for every child we spawn, which inherits
/// our error mode). `SEM_FAILCRITICALERRORS` is the one that matters here: it turns the modal
/// "The application was unable to start correctly (0xc0000142)" box — raised by the loader when a
/// child's DLL init fails — into a plain non-zero exit we can read, instead of a dialog that blocks
/// a headless/TUI agent forever. `SEM_NOGPFAULTERRORBOX` and `SEM_NOOPENFILEERRORBOX` close the
/// sibling crash/open-file dialogs. No-op off Windows. Declared via a raw `extern` so no new
/// windows-sys feature is pulled (kernel32 is always linked).
#[cfg(windows)]
fn suppress_hard_error_dialogs() {
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
    const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
    #[allow(non_snake_case)]
    extern "system" {
        fn SetErrorMode(uMode: u32) -> u32;
    }
    // SAFETY: `SetErrorMode` is a thread-safe kernel32 call taking a plain flag word; it has no
    // failure mode we need to observe (it returns the prior mode, which we don't need at startup).
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

/// No-op on non-Windows platforms — the hard-error dialog is a Windows concept.
#[cfg(not(windows))]
fn suppress_hard_error_dialogs() {}

#[tokio::main]
async fn main() -> Result<()> {
    // Restore the terminal (leave alt screen, show cursor, reset scroll region + cooked stdin) BEFORE
    // the default panic printer runs, so a panic inside retained/sticky mode never dumps its backtrace
    // into the alternate screen or onto a frame with a restricted scroll region. Idempotent.
    crate::ui::tui::install_panic_hook();
    // Windows: stop a failing CHILD process from popping a modal "unable to start correctly" box.
    // A child inherits our error mode, so this one call covers every spawn (git, cmd, sh, mcp, lsp).
    // Without it, a git.exe that fails loader-init (0xc0000142) blocks behind a dialog the agent
    // can neither see nor dismiss; with it the child just returns a status we handle. Idempotent.
    suppress_hard_error_dialogs();
    let cli = Cli::parse();
    // Before ANY command touches the store: ids the old slugifier cut mid-word are re-slugged whole.
    // Ahead of the `match` so the CLI paths (`memory list`, `memory show`) and the REPL see the same
    // ids — see `run_id_migration_once`.
    run_id_migration_once();
    let command = match cli.command {
        Some(c) => c,
        // Bare `ng` → the interactive landing menu (hermes-style).
        None => return run_menu().await,
    };
    match command {
        Commands::Chat(args) => run_chat(args).await,
        Commands::Agent(args) => run_agent_cmd(args).await,
        Commands::Workflow(args) => run_workflow_cmd(args).await,
        Commands::Memory { cmd } => run_memory(cmd).await,
        Commands::Skill { cmd } => run_skill(cmd).await,
        Commands::Persona { cmd } => run_persona(cmd),
        Commands::Soul { cmd } => run_soul(cmd),
        Commands::Bench { cmd } => match cmd {
            BenchCmd::Memory {
                split,
                update_baseline,
                hybrid,
                fuzzy,
                evolution,
            } => {
                if evolution {
                    bench::run_evolution()
                } else {
                    bench::run(&split, update_baseline, hybrid, fuzzy)
                }
            }
            BenchCmd::Profile => bench::brain::run_profile(),
            BenchCmd::Dialectic => bench::brain::run_dialectic(),
            BenchCmd::Health => bench::brain::run_health(),
            BenchCmd::Loop => bench::loop_eval::run().await,
        },
        Commands::Config { cmd } => config_ui::run_config(cmd).await,
        Commands::Auth { cmd } => config_ui::run_auth(cmd).await,
        Commands::Models(args) => run_models(args).await,
        Commands::Crawl(args) => run_crawl(args).await,
        Commands::Reach { cmd } => run_reach(cmd).await,
        Commands::Serve {
            install,
            uninstall,
            user,
            now,
            token,
            health,
            bots,
        } => {
            // The probe answers FIRST and touches nothing else: it runs every few seconds for the
            // life of the container, so it must not load/rewrite config or start any subsystem.
            // Shaped for a container `exec` probe — one line out, exit 0 healthy / 1 not. Exiting
            // directly (rather than returning an `Err`) keeps it to that single line; an `Err` from
            // `main` would add anyhow's "Error:" framing to every failed probe in the pod log.
            if health {
                std::process::exit(if hostbot::run_health_check() { 0 } else { 1 });
            }
            // `--token` = "paste and run": persist it to config before booting, so `serve --token <t>`
            // on a fresh machine works with no separate `telegram setup` step (pairing captures owner).
            if let Some(token) = token.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let mut cfg = cli_config::load();
                let mut tg = cfg.telegram.clone().unwrap_or_default();
                tg.token = Some(token.to_string());
                cfg.telegram = Some(tg);
                cli_config::save(&cfg)?;
            }
            if install || uninstall {
                hostbot::run_serve_service(install, uninstall, user, now).await
            } else {
                hostbot::run_serve(bots).await
            }
        }
        Commands::Telegram { cmd } => run_telegram(cmd).await,
        Commands::Discord { cmd } => run_discord(cmd).await,
        Commands::Time { cmd } => run_time(cmd),
        Commands::Team { cmd } => run_team(cmd),
        Commands::Work { cmd } => run_work(cmd),
        Commands::Where => {
            println!("{}", where_report());
            Ok(())
        }
        Commands::Import { path } => run_import(path).await,
        Commands::Zone { cmd } => run_zone(cmd),
        Commands::Cron { cmd } => cron::handle(cmd).await,
        Commands::Mcp { cmd } => match cmd {
            McpCmd::List => {
                println!("{}", crate::agent::mcp::summary());
                Ok(())
            }
            McpCmd::Login { name } => {
                crate::agent::mcp::login(&name).await?;
                println!(
                    "{}",
                    style(format!("✓ signed in to '{name}'. Its tools load on your next message (/mcp to verify)."))
                        .color256(splash::ACCENT)
                );
                Ok(())
            }
            McpCmd::Trust => {
                crate::agent::mcp::trust_project()?;
                println!(
                    "{}",
                    style("✓ trusted — this repo's project MCP servers will load.")
                        .color256(splash::ACCENT)
                );
                println!("{}", crate::agent::mcp::summary());
                Ok(())
            }
            McpCmd::Untrust => {
                crate::agent::mcp::untrust_project()?;
                println!(
                    "{}",
                    style("project MCP servers untrusted (no longer loaded).")
                        .color256(splash::ACCENT)
                );
                Ok(())
            }
        },
        Commands::Apps { cmd } => run_apps(cmd).await,
        Commands::Agents { cmd } => run_agents(cmd).await,
        Commands::Update => features::update::run().await,
        Commands::PromptSize { model, tools, json } => run_prompt_size(model, tools, json),
        Commands::Art => {
            crate::ui::moonscape::run();
            Ok(())
        }
    }
}

/// `aizen prompt-size` — byte breakdown of the per-turn fixed overhead (system prompt + tool
/// schemas). Offline: builds the same lanes and the same registry a real turn would, then measures
/// them. No request is made, so it costs nothing and works without a configured key.
fn run_prompt_size(model: Option<String>, show_tools: bool, as_json: bool) -> Result<()> {
    let model = model
        .or_else(|| cli_config::load().model)
        .unwrap_or_else(|| "gpt-4o".to_string());

    // Same lanes a turn would send: static base + <environment> (stable) and the memory/persona
    // blocks (dynamic). LSP is armed first so the registry advertises the symbolic-edit tools.
    let bundle = active_system_prompt_bundle(&model);
    arm_lsp_session();
    let registry = agent::builtin::default_registry_with_task(
        reqwest::Client::new(),
        String::new(),
        String::new(),
        model.clone(),
        crate::core::approval::ApprovalMode::Ask,
        resolve_ctx_window(&model).0,
        None,
    )?;
    let defs = registry.defs();
    let tools_json = serde_json::to_string(&defs)?;

    let stable = bundle.stable.len();
    let dynamic = bundle.dynamic.len();
    let prompt = stable + dynamic;
    let tools_bytes = tools_json.len();
    let total = prompt + tools_bytes;
    // Rough: 4 bytes/token. The real count is tokenizer-specific — this is a budget, not a bill.
    let tok = |b: usize| b / 4;

    let mut per_tool: Vec<(usize, String)> = defs
        .iter()
        .map(|d| {
            let n = serde_json::to_string(d).map(|s| s.len()).unwrap_or(0);
            (n, d.function.name.clone())
        })
        .collect();
    per_tool.sort_by(|a, b| b.0.cmp(&a.0));

    if as_json {
        let out = serde_json::json!({
            "model": model,
            "system_prompt": { "bytes": prompt, "stable_bytes": stable, "dynamic_bytes": dynamic },
            "tools": { "count": defs.len(), "json_bytes": tools_bytes },
            "fixed_total": { "bytes": total, "approx_tokens": tok(total) },
            "per_tool": per_tool
                .iter()
                .map(|(n, name)| serde_json::json!({ "name": name, "bytes": n }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let kb = |b: usize| format!("{:.1} KB", b as f64 / 1024.0);
    println!("Prompt-size breakdown (model={model})\n");
    println!("  System prompt        : {prompt:>8} B  ({})", kb(prompt));
    println!(
        "    stable  (base+env) : {stable:>8} B  ({})  \u{2190} cache-stable prefix",
        kb(stable)
    );
    println!("    dynamic (memory)   : {dynamic:>8} B  ({})", kb(dynamic));
    println!(
        "  Tool schemas         : {tools_bytes:>8} B  ({}, {} tools, avg {} B)",
        kb(tools_bytes),
        defs.len(),
        if defs.is_empty() {
            0
        } else {
            tools_bytes / defs.len()
        }
    );
    println!(
        "  Fixed per turn       : {total:>8} B  ({}, ~{}k tokens)",
        kb(total),
        tok(total) / 1000
    );
    if show_tools {
        println!("\n  Per tool, largest first:");
        for (n, name) in &per_tool {
            println!("    {n:>6} B  {name}");
        }
    } else {
        println!("\n  (--tools for per-tool sizes, --json for machine output)");
    }
    Ok(())
}

/// `aizen apps …` — connect apps via the MCP registry.
async fn run_apps(cmd: Option<AppsCmd>) -> Result<()> {
    match cmd {
        None | Some(AppsCmd::List) => {
            apps_print_list();
            Ok(())
        }
        Some(AppsCmd::Search { query, limit }) => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                return Err(anyhow!("usage: aizen apps search <keywords>"));
            }
            let hits = app_catalog::dedupe_latest(
                app_catalog::search(q.trim(), limit.unwrap_or(20).clamp(1, 50)).await?,
            );
            if hits.is_empty() {
                println!("no apps on {} match '{q}'", app_catalog::registry_base());
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "{} result(s) from {} — `aizen apps add <name>` to connect:",
                    hits.len(),
                    app_catalog::registry_base()
                ))
                .dim()
            );
            for s in &hits {
                let name = style(&s.name).color256(splash::ACCENT);
                println!("  {name}\n    {}", s.summary_line());
            }
            Ok(())
        }
        Some(AppsCmd::Add { name }) => apps_add(&name).await,
        Some(AppsCmd::Info { name }) => apps_info(&name).await,
        Some(AppsCmd::Login { name }) => {
            crate::agent::mcp::login(&name).await?;
            println!(
                "{}",
                style(format!("✓ signed in to '{name}'. Its tools load on your next message (/mcp to verify)."))
                    .color256(splash::ACCENT)
            );
            Ok(())
        }
        Some(AppsCmd::Remove { name }) => {
            if app_catalog::remove_server(&name)? {
                crate::agent::mcp_oauth::clear_token(&name); // drop any cached OAuth token too
                crate::agent::mcp::invalidate();
                println!(
                    "{}",
                    style(format!("✓ disconnected '{name}'.")).color256(splash::ACCENT)
                );
            } else {
                println!("no connected app keyed '{name}' (see `aizen apps list`).");
            }
            Ok(())
        }
    }
}

/// Render the featured catalog with connection badges.
fn apps_print_list() {
    let installed = app_catalog::installed_keys();
    println!(
        "{}",
        style("Apps — connect via the MCP registry (`aizen apps add <key>`):").bold()
    );
    for f in app_catalog::FEATURED {
        let on = installed.iter().any(|k| k == f.key);
        let badge = if on {
            style("✓").color256(splash::ACCENT).to_string()
        } else {
            style("○").dim().to_string()
        };
        println!(
            "  {badge}  {} {:<18} {}",
            icons::g(f.icon),
            style(f.key).color256(splash::ACCENT),
            style(f.blurb).dim()
        );
    }
    // Apps the user connected that aren't in the featured set (added via `aizen apps add <name>`).
    let custom: Vec<&String> = installed
        .iter()
        .filter(|k| !app_catalog::FEATURED.iter().any(|f| f.key == **k))
        .collect();
    if !custom.is_empty() {
        println!("\n{}", style("connected (custom):").bold());
        for k in &custom {
            println!(
                "  {}  {} {}",
                style("✓").color256(splash::ACCENT),
                icons::g("🧩"),
                style(k).color256(splash::ACCENT)
            );
        }
    }
    println!(
        "\n{}",
        style("details: `aizen apps info <key>`   ·   search: `aizen apps search <keywords>`   ·   remove: `aizen apps remove <key>`").dim()
    );
}

/// Resolve a featured key or registry name → fetch spec → pick transport → prompt secrets → write
/// the mcp.json entry. Interactive (hidden secret prompts); transparent about what it chose.
async fn apps_add(name: &str) -> Result<()> {
    let theme = ui_theme();
    // Resolve to the VIABLE candidate set and let the user CHOOSE (publisher + local/hosted shown) —
    // connecting an app hands it your token, so we never silently wire whatever sorts first. The
    // best heuristic match (pick_best) is the pre-selected default. (A featured app's vendor is just
    // a search hint + default; the official server is often OAuth-only, so community servers appear.)
    let (key0, query, prefer, label) = match app_catalog::featured(name) {
        Some(f) => (
            Some(f.key.to_string()),
            f.query.to_string(),
            f.prefer.to_string(),
            f.label.to_string(),
        ),
        None => (None, name.to_string(), name.to_string(), name.to_string()),
    };
    let hits = app_catalog::dedupe_latest(app_catalog::search(&query, 50).await?);
    let viable: Vec<app_catalog::RegistryServer> = hits
        .into_iter()
        .filter(|s| app_catalog::is_viable(s))
        .collect();
    if viable.is_empty() {
        return Err(anyhow!(
            "no connectable '{label}' server found on the registry (only legacy sse-only entries, which aizen's client doesn't speak). Run `aizen apps search {query}` to explore."
        ));
    }
    let default_idx = app_catalog::pick_best(&viable, &prefer)
        .and_then(|best| viable.iter().position(|s| s.name == best.name))
        .unwrap_or(0);
    // One clean, COLUMN-ALIGNED line per server: ★ recommended · transport · short name · short desc.
    // (The old 2-line label repeated the name and let long descriptions wrap into a wall of text.)
    let trunc = |s: &str, n: usize| -> String {
        let s = s.replace(['\n', '\r'], " ");
        if s.chars().count() <= n {
            s
        } else {
            format!(
                "{}…",
                s.chars().take(n.saturating_sub(1)).collect::<String>()
            )
        }
    };
    let name_w = viable
        .iter()
        .map(|s| s.short_name().chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 30);
    let tag_w = viable
        .iter()
        .map(|s| s.transport_tag().chars().count())
        .max()
        .unwrap_or(7)
        .clamp(7, 14);
    let mut labels: Vec<String> = viable
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let star = if i == default_idx { "★" } else { " " };
            let tag = format!("{:<w$}", trunc(&s.transport_tag(), tag_w), w = tag_w);
            let nm = format!("{:<w$}", trunc(&s.short_name(), name_w), w = name_w);
            format!("{star} {tag}  {nm}  {}", trunc(&s.description, 52))
        })
        .collect();
    labels.push("Cancel".to_string());
    println!(
        "{}",
        style(format!("Connect {label}  —  ★ recommended · local·X = your machine · sign-in = OAuth · hosted = third party")).dim()
    );
    let idx = match Select::with_theme(&theme)
        .with_prompt("Pick a server (↑↓, Enter)")
        .items(&labels)
        .default(default_idx)
        .interact_opt()?
    {
        Some(i) if i < viable.len() => i,
        _ => {
            println!("{}", style("cancelled.").dim());
            return Ok(());
        }
    };
    let server = viable[idx].clone();
    let key = key0.unwrap_or_else(|| app_catalog::slug_from_name(&server.name));

    // Runtime-aware: prefer a transport whose runner is actually on PATH (don't wire an npx server
    // when Node isn't installed if a remote would work).
    let choice = app_catalog::pick_transport_for_install(&server)
        .context("this server declares no transport aizen can use")?;
    let repo = server
        .repository
        .as_ref()
        .map(|r| r.url.clone())
        .unwrap_or_default();
    println!(
        "{}",
        style(format!("→ {}", server.name)).color256(splash::ACCENT)
    );
    if !server.description.is_empty() {
        println!("  {}", style(&server.description).dim());
    }
    if !repo.is_empty() {
        println!("  {}", style(&repo).dim());
    }
    if let Some(rt) = app_catalog::runtime_prereq(&server, choice) {
        let have = which_runtime(rt);
        let note = if have {
            format!("runs locally via {rt} (found)")
        } else {
            format!("runs locally via {rt} — NOT found on PATH; install it to run this app")
        };
        println!("  {}", style(note).dim());
    }
    // Static-token hosted remote → an explicit host-named confirm before we collect a token (it
    // leaves your machine for a third party). This is the strongest gate; refusing aborts the connect.
    if let app_catalog::TransportChoice::Remote(i) = choice {
        let host = server
            .remotes
            .get(i)
            .map(|r| app_catalog::host_of(&r.url))
            .unwrap_or_default();
        println!(
            "  {}",
            style(format!(
                "⚠ hosted remote @ {host} — a third party runs this server."
            ))
            .yellow()
        );
        let go = Confirm::with_theme(&theme)
            .with_prompt(format!(
                "Send your credentials to '{host}' (a third party)?"
            ))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !go {
            println!(
                "{}",
                style("cancelled — no third-party remote connected.").dim()
            );
            return Ok(());
        }
    }
    // OAuth remote → you authenticate directly with the vendor (no token leaves via us); confirm we
    // may open the browser to sign in.
    if let app_catalog::TransportChoice::OAuthRemote(i) = choice {
        let host = server
            .remotes
            .get(i)
            .map(|r| app_catalog::host_of(&r.url))
            .unwrap_or_default();
        println!(
            "  {}",
            style(format!(
                "🔐 sign-in app @ {host} — Aizen will open your browser to authorize."
            ))
            .dim()
        );
        let go = Confirm::with_theme(&theme)
            .with_prompt(format!("Connect '{host}' and sign in now?"))
            .default(true)
            .interact()
            .unwrap_or(false);
        if !go {
            println!("{}", style("cancelled.").dim());
            return Ok(());
        }
    }

    // Collect any declared secrets (hidden), with a confirm gate (we're writing a token to disk).
    let mut ask = |spec: &app_catalog::PromptSpec| -> String {
        let prompt = if spec.description.is_empty() {
            format!("{} ", spec.label)
        } else {
            format!("{} ({})", spec.label, spec.description)
        };
        let val = if spec.is_secret {
            Password::with_theme(&theme)
                .with_prompt(prompt.trim())
                .allow_empty_password(true)
                .interact()
                .unwrap_or_default()
        } else {
            Input::<String>::with_theme(&theme)
                .with_prompt(prompt.trim())
                .allow_empty(true)
                .interact_text()
                .unwrap_or_default()
        };
        val.trim().to_string()
    };
    let entry = app_catalog::build_entry(&server, choice, &mut ask)?;

    // Confirm gate (we're about to write a token to disk) — show the resolved entry with secrets
    // MASKED so the user sees exactly what gets written before committing.
    println!("\n{}", style(format!("About to connect '{key}':")).bold());
    print_entry_summary(&entry, Some(&key));
    let ok = Confirm::with_theme(&theme)
        .with_prompt("Write this to mcp.json?")
        .default(true)
        .interact()
        .unwrap_or(false);
    if !ok {
        println!("{}", style("cancelled — nothing written.").dim());
        return Ok(());
    }
    app_catalog::write_server(&key, entry)?;
    crate::agent::mcp::invalidate(); // hot-reload: the next message reconnects from the new mcp.json

    // OAuth app → run the browser sign-in right now so it's usable immediately. A failure isn't fatal:
    // the entry is written, the user can retry with `aizen apps login <key>`.
    if matches!(choice, app_catalog::TransportChoice::OAuthRemote(_)) {
        match crate::agent::mcp::login(&key).await {
            Ok(()) => println!(
                "{}",
                style(format!("✓ connected & signed in to '{key}'. Its tools load on your next message (/mcp to verify)."))
                    .color256(splash::ACCENT)
            ),
            Err(e) => println!(
                "{}",
                style(format!("connected '{key}', but sign-in didn't finish — {e:#}\n  finish it with `aizen apps login {key}`."))
                    .yellow()
            ),
        }
        return Ok(());
    }
    println!(
        "{}",
        style(format!(
            "✓ connected '{key}'.  Its tools load on your next message (/mcp to verify)."
        ))
        .color256(splash::ACCENT)
    );
    Ok(())
}

/// Print an mcp.json entry's transport + config with secret VALUES masked (shared by the add-confirm
/// preview and `apps info`). Never prints a token value — presence only. `key` (when known) lets it
/// show OAuth sign-in state from the token cache.
fn print_entry_summary(entry: &serde_json::Value, key: Option<&str>) {
    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
        println!("  {} remote (streamable-http)", style("transport").dim());
        println!("  {} {url}", style("url      ").dim());
        println!(
            "  {} {}",
            style("host     ").dim(),
            style(app_catalog::host_of(url)).dim()
        );
        if entry.get("auth").and_then(|v| v.as_str()) == Some("oauth") {
            let signed = key.map(crate::agent::mcp_oauth::has_token).unwrap_or(false);
            let state = if signed {
                "signed in".to_string()
            } else {
                "not signed in — `aizen apps login <key>`".to_string()
            };
            println!("  {} oauth ({state})", style("auth     ").dim());
        }
    } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        let args = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        println!("  {} local (stdio)", style("transport").dim());
        println!("  {} {cmd} {args}", style("command  ").dim());
        if !cmd.contains(['/', '\\']) {
            let have = which_runtime(cmd);
            let note = if have {
                format!("{cmd}: found on PATH")
            } else {
                format!("{cmd}: NOT on PATH — install it to run this app")
            };
            println!("  {} {note}", style("runtime  ").dim());
        }
    }
    for field in ["env", "headers"] {
        if let Some(obj) = entry.get(field).and_then(|v| v.as_object()) {
            for (k, v) in obj {
                println!(
                    "  {} {k} = {}",
                    style(format!("{field:<8}")).dim(),
                    mask_secret(v.as_str().unwrap_or(""))
                );
            }
        }
    }
}

/// Mask a secret/config value for display: presence only, never the value (the standing key-safety
/// rule). Empty → "(empty)"; set → "•••• (set)".
fn mask_secret(v: &str) -> String {
    if v.trim().is_empty() {
        style("(empty)").dim().to_string()
    } else {
        style("•••• (set)").dim().to_string()
    }
}

/// `aizen apps info <key>` — the detail view for ONE connected app: its mcp.json config (transport +
/// secrets MASKED) plus a LIVE probe (handshake + the tools it actually exposes, or why it failed).
async fn apps_info(key: &str) -> Result<()> {
    let Some(entry) = app_catalog::installed_entry(key) else {
        return Err(anyhow!(
            "no connected app keyed '{key}' — see `aizen apps list`"
        ));
    };
    println!("{}", style(key).color256(splash::ACCENT).bold());
    print_entry_summary(&entry, Some(key));

    // Live probe.
    println!("  {}", style("probing (connect + tools/list)…").dim());
    match crate::agent::mcp::probe(key).await {
        Ok(rep) => {
            let info = rep.server_info.get("serverInfo");
            let sname = info
                .and_then(|s| s.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(key);
            let sver = info
                .and_then(|s| s.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!(
                "  {} {}",
                style("✓").color256(splash::ACCENT),
                style(format!("{sname} {sver}  ·  {} tool(s)", rep.tools.len())).bold()
            );
            for t in &rep.tools {
                let ro = if t.read_only {
                    style(" [read-only]").dim().to_string()
                } else {
                    String::new()
                };
                let d: String = t.description.chars().take(72).collect();
                println!(
                    "    {}{ro}  {}",
                    style(&t.name).color256(splash::ACCENT),
                    style(d).dim()
                );
            }
            if rep.tools.is_empty() {
                println!("    {}", style("(this server advertised no tools)").dim());
            }
        }
        // `{e:#}` = the full anyhow chain (includes the server's stderr tail captured by the client).
        Err(e) => println!("  {}", style(format!("✗ could not connect — {e:#}")).red()),
    }
    Ok(())
}

/// Best-effort PATH check for a runner (npx/uvx/docker) — Windows adds `.cmd`/`.exe` variants.
fn which_runtime(rt: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".cmd", ".exe", ".bat"]
    } else {
        &[""]
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            if dir.join(format!("{rt}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

async fn run_crawl(args: CrawlArgs) -> Result<()> {
    let opts = crawl::CrawlOptions {
        seeds: args.urls,
        max_depth: args.depth,
        max_pages: args.max_pages,
        scope: crawl::Scope::parse(&args.scope)?,
        concurrency: args.concurrency,
        timeout_secs: args.timeout,
    };
    let http = http_client()?;
    let report = crawl::crawl(&http, &opts).await.context("crawl failed")?;

    if args.json {
        let arr: Vec<serde_json::Value> = report
            .found
            .iter()
            .map(|f| serde_json::json!({"url": f.url, "depth": f.depth, "via": f.via.tag()}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        for f in &report.found {
            if args.show_source {
                println!(
                    "{}  {}",
                    f.url,
                    style(format!("[{} d{}]", f.via.tag(), f.depth)).dim()
                );
            } else {
                println!("{}", f.url);
            }
        }
    }
    eprintln!(
        "{}",
        style(format!(
            "crawled {} page(s) → {} URL(s)",
            report.pages_fetched,
            report.found.len()
        ))
        .dim()
    );
    Ok(())
}

/// `aizen reach doctor [--json]` / `aizen reach status` — the web-access health check.
async fn run_reach(cmd: ReachCmd) -> Result<()> {
    match cmd {
        ReachCmd::Status => {
            println!("{}", crate::agent::reach::render_passive());
        }
        ReachCmd::Doctor { json } => {
            if !json {
                eprintln!("{}", style("probing every backend (a few seconds)…").dim());
            }
            let reports = crate::agent::reach::doctor().await;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::agent::reach::report_json(&reports))?
                );
            } else {
                println!("{}", crate::agent::reach::render_report(&reports));
            }
        }
    }
    Ok(())
}

/// Run the agent loop once (non-streaming, quiet) and return its final text — used by `aizen serve`
/// to answer a Telegram message.
async fn run_agent_capture(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    task: &str,
    approval_mode: ApprovalMode,
) -> Result<String> {
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        model,
        Some(&frozen),
    );
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        resolve_ctx_window(model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let cfg = AgentConfig {
        approval_mode,
        quiet: true,
        enable_verify_gate: false,
        ..Default::default()
    };

    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        client::chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
    };
    let outcome = agent::run_agent(chat, &cfg, &registry, &system, task).await?;
    // A `clarify` yield in a captured (non-REPL) run — e.g. `aizen serve` — has no input box to loop
    // back to, so surface the question as the reply itself. Over Telegram the owner just answers
    // with their next message; for a plain capture caller it reads as the agent's question.
    if let StopReason::AwaitingInput(q) = &outcome.stop {
        return Ok(format!("❓ {q}"));
    }
    Ok(outcome
        .final_text
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(the agent produced no answer)".to_string()))
}

/// Strip URL userinfo before display when it carries a password/token
/// (`https://user:TOKEN@host/…`) — remote URLs may embed credentials and the identity surfaces
/// must never print one. A plain username (`git@host:…`) is kept: it isn't a secret and losing
/// it would make the URL unrecognizable.
fn redact_remote_url(url: &str) -> String {
    let (scheme, rest) = match url.find("://") {
        Some(i) => url.split_at(i + 3),
        None => ("", url),
    };
    match rest.find('@') {
        Some(at) if rest[..at].contains(':') => format!("{scheme}***@{}", &rest[at + 1..]),
        _ => url.to_string(),
    }
}

/// How many `*.md` files a store directory holds, and a `(not created yet)` note when it doesn't
/// exist. Shared by both `where` reports so an absent folder never reads as an empty one.
fn dir_count_line(label: &str, p: &std::path::Path, unit: &str) -> String {
    if !p.exists() {
        return format!("  {label:<8}: {}   (not created yet)", p.display());
    }
    let n = std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                .count()
        })
        .unwrap_or(0);
    format!("  {label:<8}: {}   {n} {unit}", p.display())
}

/// Where the memory store physically lives, per directory, with counts.
///
/// `memory list` names three commands and every one of them edits a SINGLE entry by id. Bulk work —
/// deleting forty near-duplicates, fixing a wrong word across many facts — is a file-manager job, and
/// until now the only place any path appeared was `memory show <id>`'s `file:` line, one entry at a
/// time. Naming the review dir matters most: 29 queued candidates sat there unreadable because
/// nothing said they were on disk at all.
fn memory_where_report() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("entries", &crate::core::config::entries_dir(), "fact(s)")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("review", &crate::core::config::review_dir(), "awaiting")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("archive", &crate::core::config::archive_dir(), "retired")
    );
    let graph = crate::core::config::graph_path();
    let edges = std::fs::read_to_string(&graph)
        .map(|r| r.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    let _ = writeln!(s, "  {:<8}: {}   {edges} edge(s)", "graph", graph.display());
    let _ = writeln!(
        s,
        "  {:<8}: {}",
        "core",
        crate::core::config::style_path().display()
    );
    let _ = write!(
        s,
        "{}",
        style(
            "\nEdit or delete files directly — they are plain markdown with a frontmatter header.\n\
             Re-run `aizen memory doctor` afterwards to catch anything left dangling."
        )
        .dim()
    );
    s
}

/// Where skills are read from — all three roots, because `skill list`'s `[project]`/`[repo]` tags
/// say which root a row came from without saying where that root is, and auto-learned skills land in
/// the zone dir whose slug (`p/admin-5296147b`) is not guessable from the project name.
fn skill_where_report() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("global", &crate::skills::skills_dir(), "skill(s)")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("zone", &crate::skills::project_zone_dir(), "skill(s)")
    );
    let _ = writeln!(
        s,
        "  {}",
        style(format!(
            "         ↑ auto-learned skills for zone {}",
            crate::core::config::project_slug()
        ))
        .dim()
    );
    let _ = write!(
        s,
        "{}",
        dir_count_line("repo", &crate::skills::project_skills_dir(), "skill(s)")
    );
    s
}

/// The identity card — one honest surface for the questions that previously had none: which
/// root am I in, which zone does my memory go to, which git binary runs, where do sessions live.
/// Shared verbatim by `aizen where` (println) and `/where` (tui::emit_line).
fn where_report() -> String {
    use std::fmt::Write as _;
    let root = crate::core::config::project_root();
    let slug = crate::core::config::project_slug();
    let home = crate::core::config::aizen_home();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let mut s = String::new();
    let _ = writeln!(s, "project root : {}", root.display());
    if let Ok(over) = std::env::var("AIZEN_PROJECT_ROOT") {
        if !over.trim().is_empty() {
            let _ = writeln!(
                s,
                "               (root forced by AIZEN_PROJECT_ROOT={})",
                over.trim()
            );
        }
    }
    let _ = writeln!(
        s,
        "cwd          : {cwd}   (identity follows the root, fixed at launch)"
    );
    let _ = writeln!(
        s,
        "zone slug    : {slug}   (keys memory scope · skills · codebase index · frozen core)"
    );
    if let Some(url) = crate::core::config::git_remote_origin(&root) {
        let _ = writeln!(
            s,
            "git remote   : {}   (informational — no longer part of the identity key)",
            redact_remote_url(&url)
        );
    }
    match crate::core::gitx::git_exe() {
        Some(p) => {
            let _ = writeln!(s, "git          : {}", p.display());
        }
        None => {
            let _ = writeln!(
                s,
                "git          : NOT FOUND — identity uses the nearest .git marker (or this folder); time-machine checkpoints are off"
            );
        }
    }
    if let Some(note) = crate::core::gitx::resolution_note() {
        if crate::core::gitx::git_exe().is_some() {
            let _ = writeln!(s, "               ({note})");
        }
    }
    let zone_dir = crate::skills::project_zone_dir();
    let idx = crate::core::config::codebase_index_path(&slug);
    let exists = |p: &std::path::Path| {
        if p.exists() {
            ""
        } else {
            "   (not created yet)"
        }
    };
    let _ = writeln!(s, "home         : {}", home.display());
    let _ = writeln!(
        s,
        "memory store : {}",
        crate::core::config::cli_memory_dir().display()
    );
    let _ = writeln!(
        s,
        "skills zone  : {}{}",
        zone_dir.display(),
        exists(&zone_dir)
    );
    let _ = writeln!(s, "codebase idx : {}{}", idx.display(), exists(&idx));
    let _ = writeln!(s, "sessions     : {}", sessions_dir().display());
    if let Some(n) = sessions_with_secrets() {
        let _ = writeln!(
            s,
            "⚠ secrets     : {n} saved transcript(s) contain credential-shaped text — a key pasted into a chat is stored verbatim. Open the folder above and edit or delete them."
        );
    }
    if let Some(l) = crate::features::zones::quick_legacy_probe() {
        let _ = writeln!(
            s,
            "⚠ legacy zone : {l} — data from the old slug keying; `aizen zone migrate` shows what would merge (--apply to do it)"
        );
    }
    s.trim_end().to_string()
}

/// How many saved transcripts hold credential-shaped text, or `None` when none do.
///
/// Names are guarded at derivation (see [`suggest_session_name`]), but a key pasted into a chat is
/// still in that file's message text: a saved session is a verbatim transcript, and nothing redacts
/// it on the way to disk. Deleting or rewriting a user's own conversation history is not a call this
/// tool makes on its own, so `/where` reports the count and names the folder — the number is
/// actionable, and the values are never printed.
///
/// Uses the vendor-prefix test, NOT the shape test that guards name derivation. Measured on the 27
/// real transcripts here: prefix matches 12 strings, all real keys; shape matched 5170, of which 4026
/// were ISO timestamps (long, mixed-case, letters and digits — indistinguishable from key material by
/// shape alone). A warning that fires on every file teaches the user to ignore it.
///
/// Counts FILES, not occurrences: the useful signal is "which files do I need to open".
fn sessions_with_secrets() -> Option<usize> {
    let rd = std::fs::read_dir(sessions_dir()).ok()?;
    let n = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter(|e| {
            std::fs::read_to_string(e.path()).is_ok_and(|raw| {
                raw.split(|c: char| c.is_whitespace() || matches!(c, '"' | ',' | '\\'))
                    .any(crate::core::slug::has_vendor_key_prefix)
            })
        })
        .count();
    (n > 0).then_some(n)
}

fn run_zone(cmd: ZoneCmd) -> Result<()> {
    match cmd {
        ZoneCmd::Migrate { apply } => {
            let plan = crate::features::zones::plan()?;
            println!("current zone: {}", plan.current_slug);
            if plan.legacy.is_empty() {
                println!("no legacy zones found for this project — nothing to merge.");
                return Ok(());
            }
            println!("legacy zones of this project:");
            for z in &plan.legacy {
                println!("  {}", z.summary());
            }
            if !apply {
                println!("\ndry-run — nothing was changed. Re-run with `aizen zone migrate --apply` to merge into {}.", plan.current_slug);
                return Ok(());
            }
            let rep = crate::features::zones::apply(&plan);
            for a in &rep.actions {
                println!("  ✓ {a}");
            }
            for w in &rep.warnings {
                eprintln!("  ⚠ {w}");
            }
            println!(
                "merged {} legacy zone(s) into {}: {} action(s), {} warning(s).",
                plan.legacy.len(),
                plan.current_slug,
                rep.actions.len(),
                rep.warnings.len()
            );
            if !rep.warnings.is_empty() {
                anyhow::bail!("zone migrate finished with warnings — each one above states exactly what moved and what didn't");
            }
            Ok(())
        }
    }
}

// ───────────────────────────── time machine (git snapshots) ─────────────────────────────

/// `aizen team …` — the non-interactive twin of `/team`. Same registry, same plan, `println!` instead
/// of `tui::emit_line` (no retained frame is up here, so a raw print is the correct surface).
fn run_team(cmd: TeamCmd) -> Result<()> {
    match cmd {
        TeamCmd::Status => {
            for line in team_status_lines() {
                println!("{line}");
            }
            Ok(())
        }
        TeamCmd::Diff { session, patch } => {
            let view = coop::resolve(&session)?;
            let reports = coop::session_diff(&view, patch)?;
            if reports.is_empty() {
                println!(
                    "{}",
                    style(format!(
                        "session {} has no file changes on disk",
                        view.manifest.session_id
                    ))
                    .dim()
                );
                return Ok(());
            }
            for report in &reports {
                for line in diff_lines(report, "--patch off") {
                    println!("{line}");
                }
            }
            Ok(())
        }
        TeamCmd::Claims => {
            let claims = coop::claims();
            if claims.is_empty() {
                println!("{}", style("no path claims recorded yet").dim());
                return Ok(());
            }
            for (path, claim) in claims {
                println!("  {path}  ← {}", style(&claim.session_id).dim());
            }
            for o in coop::overlaps() {
                println!(
                    "  {} {}  ({} → {})",
                    style("⚠").color256(theme::WARN),
                    o.path,
                    o.first,
                    o.second
                );
            }
            Ok(())
        }
        TeamCmd::Commit {
            session,
            message,
            dry_run,
            force,
            yes,
        } => {
            let view = coop::resolve(&session)?;
            let plan = coop::plan_commit(&view)?;
            for b in &plan.blockers {
                println!("{} {b}", style("⚠").color256(theme::WARN));
            }
            if !plan.blockers.is_empty() && !force {
                bail!("refusing to commit; nothing was staged (re-run with --force to override)");
            }
            if !plan.shared_paths.is_empty() {
                println!(
                    "{} {} file(s) are shared with another session — committing them carries that \
                     session's edits to the SAME file along: {}",
                    style("⚠").color256(theme::WARN),
                    plan.shared_paths.len(),
                    plan.shared_paths.join(", ")
                );
            }
            let review = coop::stage_plan(&plan)?;
            for line in review.stat.lines() {
                println!("  {line}");
            }
            if !review.separated.is_empty() {
                println!(
                    "{} {} shared file(s) were separated: the commit holds only this session's \
                     version, while the working tree keeps both sessions' edits: {}",
                    style("↔").color256(splash::ACCENT),
                    review.separated.len(),
                    review.separated.join(", ")
                );
            }
            // `--yes` is the only path to an actual commit: a bare `aizen team commit` reviews and
            // rolls the index back, so discovering the command cannot land one.
            if dry_run || !yes {
                coop::unstage_plan(&plan)?;
                println!(
                    "{}",
                    style(
                        "review only — unstaged again, nothing was committed (add --yes to commit)"
                    )
                    .dim()
                );
                return Ok(());
            }
            let msg = message.unwrap_or_else(|| {
                let task = view.manifest.task.trim();
                if task.is_empty() {
                    format!("aizen session {}", plan.session_id)
                } else {
                    task.to_string()
                }
            });
            let out = coop::commit_staged(&plan, &msg, &review)?;
            for line in out.lines().take(6) {
                println!("  {line}");
            }
            Ok(())
        }
    }
}

/// `aizen work …` — isolated worktrees, for when two windows should not share one tree at all.
fn run_work(cmd: WorkCmd) -> Result<()> {
    match cmd {
        WorkCmd::New { name } => {
            let wt = coop::work_new(&name)?;
            println!(
                "{} {}\n  branch {}\n  open a session there:  cd {} && aizen",
                style("✓ worktree").color256(splash::ACCENT),
                wt.path.display(),
                wt.branch,
                wt.path.display()
            );
            Ok(())
        }
        WorkCmd::List => {
            let all = coop::work_list()?;
            if all.is_empty() {
                println!(
                    "{}",
                    style("no aizen worktrees — create one with `aizen work new <name>`").dim()
                );
                return Ok(());
            }
            for wt in &all {
                let mut notes = Vec::new();
                if wt.dirty {
                    notes.push("dirty".to_string());
                }
                if wt.ahead > 0 {
                    notes.push(format!("{} unmerged commit(s)", wt.ahead));
                }
                if wt.sessions > 0 {
                    notes.push(format!("{} live session(s)", wt.sessions));
                }
                let tail = if notes.is_empty() {
                    style("clean".to_string()).dim().to_string()
                } else {
                    style(notes.join(" · ")).color256(theme::WARN).to_string()
                };
                println!("  {:<20} {:<24} {tail}", wt.name, wt.branch);
                println!("    {}", style(wt.path.display().to_string()).dim());
            }
            Ok(())
        }
        WorkCmd::Remove { name, force } => {
            let msg = coop::work_remove(&name, force)?;
            println!("{} {msg}", style("✓").color256(splash::ACCENT));
            Ok(())
        }
    }
}

fn run_time(cmd: TimeCmd) -> Result<()> {
    match cmd {
        TimeCmd::Save { label } => {
            let snap = timemachine::save(&label.join(" "), false)?;
            println!(
                "{} #{}  {}",
                style("✓ checkpoint").color256(splash::ACCENT),
                snap.id,
                style(&snap.created).dim()
            );
            Ok(())
        }
        TimeCmd::List => {
            print_timeline()?;
            Ok(())
        }
        TimeCmd::Restore { id } => {
            let snap = timemachine::restore(id)?;
            let label = if snap.label.is_empty() {
                "(no label)".to_string()
            } else {
                snap.label.clone()
            };
            println!(
                "{} #{} — {label}",
                style("⏪ restored to").color256(splash::ACCENT),
                snap.id
            );
            // Say WHAT changed and that it's undoable: aizen only rewinds the working tree (files),
            // never your chat/history — and because the pre-restore state was auto-snapshotted, you
            // can always go forward again (`aizen time redo`, or restore the newest checkpoint).
            println!("{}", style("  files only — your conversation is untouched · reversible with `aizen time redo`").dim());
            Ok(())
        }
        TimeCmd::Diff {
            from,
            to,
            patch,
            paths,
            json,
        } => run_time_diff(from, to, paths, patch, json),
        TimeCmd::Undo => {
            let snap = timemachine::undo()?;
            println!(
                "{} #{}",
                style("⏪ undo →").color256(splash::ACCENT),
                snap.id
            );
            Ok(())
        }
        TimeCmd::Redo => {
            let snap = timemachine::redo()?;
            println!(
                "{} #{}",
                style("⏩ redo →").color256(splash::ACCENT),
                snap.id
            );
            Ok(())
        }
        TimeCmd::Prune { keep } => {
            let k = keep.or(cli_config::load().timemachine_keep).unwrap_or(50);
            let dropped = timemachine::prune(k)?;
            println!(
                "{} {dropped} old checkpoint(s); kept ≤{k}.",
                style("🧹 pruned").color256(splash::ACCENT)
            );
            Ok(())
        }
        TimeCmd::Doctor { json, repair } => {
            let report = if repair {
                timemachine::doctor_repair()?
            } else {
                timemachine::doctor()?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}  repo {} · worktree {} · {} checkpoint(s)",
                    if report.ok {
                        "✓ time machine healthy"
                    } else {
                        "⚠ time machine needs attention"
                    },
                    report.repo_id,
                    report.worktree_id,
                    report.checkpoints
                );
                println!("  store {}", report.store);
                for issue in &report.issues {
                    println!("  - {issue}");
                }
            }
            if !report.ok {
                bail!("time-machine doctor found {} issue(s)", report.issues.len());
            }
            Ok(())
        }
        TimeCmd::Gc { all, apply } => {
            if all {
                let report = timemachine::gc_all(apply)?;
                let fmt_mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
                if report.orphans.is_empty() {
                    println!(
                        "{} {} store(s) scanned · no orphans (every source repo still exists)",
                        style("🧹 time gc --all:").color256(splash::ACCENT),
                        report.stores.len()
                    );
                } else {
                    let total: u64 = report.orphans.iter().map(|o| o.bytes).sum();
                    println!(
                        "{} {} orphan store(s), {} reclaimable:",
                        style("🧹 time gc --all:").color256(splash::ACCENT),
                        report.orphans.len(),
                        fmt_mb(total)
                    );
                    for o in &report.orphans {
                        println!(
                            "  {} · {} · {} checkpoint(s) · source gone: {}",
                            o.repo_id,
                            fmt_mb(o.bytes),
                            o.checkpoints,
                            o.source.as_deref().unwrap_or("(unknown)")
                        );
                    }
                    if report.applied {
                        println!(
                            "  → moved to {}",
                            report.trash_dir.as_deref().unwrap_or("(trash)")
                        );
                    } else {
                        println!(
                            "  (dry-run — re-run with {} to move these to .trash/)",
                            style("--apply").bold()
                        );
                    }
                }
                Ok(())
            } else {
                let report = timemachine::doctor_gc()?;
                println!(
                    "{} repo {} · worktree {} · {} checkpoint(s)",
                    style("🧹 time metadata cleaned:").color256(splash::ACCENT),
                    report.repo_id,
                    report.worktree_id,
                    report.checkpoints
                );
                Ok(())
            }
        }
        TimeCmd::Clear => {
            let n = timemachine::clear()?;
            println!(
                "{} {n} checkpoint(s) deleted.",
                style("🧹 cleared").color256(splash::ACCENT)
            );
            Ok(())
        }
    }
}

/// Resolve the `[FROM] [TO]` positional pair into two timeline sides.
///
/// The defaults encode the question people actually ask. Bare `time diff` means "what have I changed
/// since the last checkpoint" (cursor → working tree), which is the state you want before deciding
/// whether to keep or rewind. One argument means "since THAT point" (given → working tree), because
/// naming a single checkpoint and getting a checkpoint↔checkpoint diff against an unnamed second
/// point would be guesswork.
fn resolve_diff_sides(
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(timemachine::DiffSide, timemachine::DiffSide)> {
    use timemachine::DiffSide;
    let parse = |s: &str| {
        DiffSide::parse(s).with_context(|| {
            format!("`{s}` is not a checkpoint id or `working` (try `aizen time list`)")
        })
    };
    match (from, to) {
        (None, _) => {
            let (snaps, cursor) = timemachine::timeline()?;
            let cur = cursor.and_then(|i| snaps.get(i)).map(|s| s.id).context(
                "no checkpoints yet — nothing to diff against (`aizen time save` first)",
            )?;
            Ok((DiffSide::Checkpoint(cur), DiffSide::Working))
        }
        (Some(f), None) => Ok((parse(f)?, DiffSide::Working)),
        (Some(f), Some(t)) => Ok((parse(f)?, parse(t)?)),
    }
}

/// Render a diff report as display lines. Shared so `aizen time diff` (stdout) and `/diff` (the TUI,
/// which MUST go through `tui::emit_line` or the render thread wipes the output) format identically.
/// `narrow_hint` differs between the two because the flag spelling does: `--path p` vs `-- p`.
fn diff_lines(report: &timemachine::DiffReport, narrow_hint: &str) -> Vec<String> {
    if report.is_empty() {
        return vec![style(format!(
            "⎇ no changes between {} and {}",
            report.from, report.to
        ))
        .dim()
        .to_string()];
    }
    let mut out = vec![format!(
        "{}  {} → {}  ·  {} file(s), {}",
        style("⎇ diff").color256(splash::ACCENT).bold(),
        report.from,
        report.to,
        report.files.len(),
        style(format!(
            "+{} -{}",
            report.total_added(),
            report.total_deleted()
        ))
        .dim(),
    )];
    for f in &report.files {
        // `None` counts mean git reported `-`: a binary file, not a zero-line change.
        let churn = match (f.added, f.deleted) {
            (Some(a), Some(d)) => format!("+{a} -{d}"),
            _ => "binary".to_string(),
        };
        let path = match &f.old_path {
            Some(old) => format!("{old} → {}", f.path),
            None => f.path.clone(),
        };
        out.push(format!("  {} {path}  {}", f.status, style(churn).dim()));
    }
    match &report.patch {
        Some(text) => {
            out.push(String::new());
            out.extend(text.lines().map(|l| l.to_string()));
            if report.patch_truncated {
                out.push(
                    style(format!("… patch truncated — narrow it with {narrow_hint}"))
                        .dim()
                        .to_string(),
                );
            }
        }
        None => out.push(
            style(format!(
                "  --patch for the full text · {narrow_hint} to narrow it"
            ))
            .dim()
            .to_string(),
        ),
    }
    out
}

/// `aizen time diff` — print the changes between two points in the timeline.
fn run_time_diff(
    from: Option<String>,
    to: Option<String>,
    paths: Vec<String>,
    patch: bool,
    json: bool,
) -> Result<()> {
    let report = build_time_diff(from, to, paths, patch)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    for line in diff_lines(&report, "--path <p>") {
        println!("{line}");
    }
    Ok(())
}

/// Cap on emitted patch bytes. Generous enough for a real review, bounded so a huge rewrite cannot
/// flood the terminal (or, for the agent tool, the tool-result budget).
const DIFF_PATCH_LIMIT: usize = 400 * 1024;

fn build_time_diff(
    from: Option<String>,
    to: Option<String>,
    paths: Vec<String>,
    patch: bool,
) -> Result<timemachine::DiffReport> {
    let (a, b) = resolve_diff_sides(from.as_deref(), to.as_deref())?;
    timemachine::diff(&a, &b, &paths, patch.then_some(DIFF_PATCH_LIMIT))
}

/// Human "2m ago" from a snapshot's stored LOCAL timestamp. Pure core (`rel_time_from`) takes `now`
/// so the bucketing is unit-testable; a malformed timestamp degrades to the raw string.
fn rel_time(created: &str) -> String {
    rel_time_from(created, chrono::Local::now().naive_local())
}
fn rel_time_from(created: &str, now: chrono::NaiveDateTime) -> String {
    match chrono::NaiveDateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S") {
        Ok(t) => {
            let secs = (now - t).num_seconds();
            if secs < 0 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86_400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86_400)
            }
        }
        Err(_) => created.to_string(),
    }
}

#[cfg(test)]
#[path = "tests/rel_time.rs"]
mod rel_time_tests;

/// `aizen time list` — a static, glanceable print of the checkpoint timeline (newest first), with the
/// active point marked `▸`, relative times, labels, and `auto`/`+chat` tags. Read-only, and CLI-only:
/// in the REPL `/timemachine` shows the same history as a picker, so there is nothing to print.
fn print_timeline() -> Result<()> {
    let (snaps, cursor) = timemachine::timeline()?;
    if snaps.is_empty() {
        tui::emit_line(
            &style("⎇ timeline — no checkpoints yet · /checkpoint to save one")
                .dim()
                .to_string(),
        );
        return Ok(());
    }
    let n = snaps.len();
    tui::emit_line(&format!(
        "{}  {n} checkpoint(s)",
        style("⎇ timeline").color256(splash::ACCENT).bold(),
    ));
    // Align the `#id` column to the widest id present (+1 for the leading `#`).
    let id_w = snaps
        .iter()
        .map(|s| s.id.to_string().len())
        .max()
        .unwrap_or(1)
        + 1;
    // Newest first (the ledger stores oldest → newest).
    for (i, s) in snaps.iter().enumerate().rev() {
        let is_cur = Some(i) == cursor;
        let id = format!("#{}", s.id);
        let rel = rel_time(&s.created);
        let label = if s.label.is_empty() {
            "(no label)".to_string()
        } else {
            s.label.clone()
        };
        let mut tags = String::new();
        if s.auto {
            tags.push_str(" · auto");
        }
        if s.has_chat {
            tags.push_str(" · +chat");
        }
        let head = format!("{id:<id_w$}  {rel:<9}  {label}");
        let mark = if is_cur { "▸" } else { " " };
        // Current point accented; tags always dim. Style the marker+head as one segment, then append
        // the dim tags separately so no ANSI code nests inside another.
        let body = if is_cur {
            format!(
                "{} {}",
                style(mark).color256(splash::ACCENT).bold(),
                style(head).color256(splash::ACCENT)
            )
        } else {
            format!("{mark} {head}")
        };
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            style(tags).dim().to_string()
        };
        tui::emit_line(&format!("{body}{tag_str}"));
    }
    tui::emit_line(
        &style("▸ = current · restore: aizen time restore <id>   (or /timemachine in the REPL)")
            .dim()
            .to_string(),
    );
    Ok(())
}

/// `/timemachine` — the whole time machine in one list: every checkpoint, and picking one rewinds to
/// that state.
///
/// A row carries the id, how long ago it was taken, its label, and the `+chat` tag when the
/// conversation was captured alongside the tree. Picking a row restores everything that checkpoint
/// holds — the working tree always, and the conversation too whenever a chat sidecar exists — so one
/// pick returns you to that code AND that chat. There is deliberately no Files/Task/Both sub-menu and
/// no `pick`/`restore` argument: this list IS the surface.
///
/// Every restore is reversible: the pre-restore tree is auto-snapshotted, and the live conversation is
/// saved to its own session file before being replaced.
async fn timemachine_menu(history: &mut Vec<Message>, model_label: &mut String) -> Result<()> {
    let theme = ui_theme();
    loop {
        let (snaps, cursor) = match timemachine::timeline() {
            Ok(t) => t,
            Err(e) => {
                println!("{e}");
                return Ok(());
            }
        };
        let n = snaps.len();
        let mut items: Vec<String> = snaps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let here = if Some(i) == cursor { "▸ " } else { "  " };
                let label = if s.label.is_empty() {
                    "(no label)".to_string()
                } else {
                    s.label.clone()
                };
                // Spell out what a pick will rewind, since the pick itself is now the whole gesture.
                let scope = if s.has_chat {
                    "code + chat"
                } else {
                    "code only"
                };
                let tags = if s.auto {
                    format!(" · auto · {scope}")
                } else {
                    format!(" · {scope}")
                };
                format!(
                    "{here}#{} {}  {label}{}",
                    s.id,
                    style(rel_time(&s.created)).dim(),
                    style(tags).dim(),
                )
            })
            .collect();
        items.push("✚ Save a checkpoint now (code + chat)".to_string());
        items.push("Back".to_string());
        let prompt = format!(
            "Time machine — {n} checkpoint(s); pick one to rewind to it (reversible). Esc to go back"
        );
        let pick = match Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(&items)
            .default(cursor.unwrap_or(0))
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            restore_checkpoint(&snaps[pick], history, model_label)?;
        } else if pick == n {
            let label: String = Input::with_theme(&theme)
                .with_prompt("Label (optional)")
                .allow_empty(true)
                .interact_text()?;
            // Capture the conversation alongside the tree so this checkpoint supports task restore.
            match timemachine::save_with_chat(label.trim(), false, history) {
                Ok(s) => println!(
                    "{} #{} ({})",
                    style("✓ checkpoint").color256(splash::ACCENT),
                    s.id,
                    if s.has_chat {
                        "code + chat"
                    } else {
                        "files only"
                    }
                ),
                Err(e) => println!("{}", style(format!("save failed: {e}")).red()),
            }
        } else {
            return Ok(());
        }
    }
}

/// Rewind to everything a checkpoint holds: the working tree, plus the conversation when that
/// checkpoint captured one.
///
/// There is no Files / Task / Both question any more — picking a row in the time machine means "put me
/// back there", and what that restores is a property of the checkpoint, not a choice to re-litigate.
/// A `/checkpoint` (or one saved from the picker) carries a chat sidecar and rewinds code + chat; an
/// auto/agent checkpoint has no sidecar and rewinds code only, which the row already says.
fn restore_checkpoint(
    snap: &timemachine::Snapshot,
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> Result<()> {
    // Files-only checkpoints (auto/agent) have no chat to restore.
    if !snap.has_chat {
        return files_restore(snap.id);
    }
    // Preflight and durably back up chat BEFORE files move. The live `history` is only assigned
    // after file restore succeeds, so a failed files phase cannot leave files/chat divergent.
    let chat = timemachine::load_chat_checked(snap.id)?;
    if chat.is_empty() {
        bail!("checkpoint #{} has an empty saved conversation", snap.id);
    }
    let backup = current_session_slug().unwrap_or_else(|| allocate_session_slug(history));
    save_session(history, &backup, Some(model_label))
        .context("backing up the current conversation before restore")?;
    files_restore(snap.id)?;
    *history = chat;
    migrate_legacy_prompt_lanes(history, model_label);
    refresh_prompt_lanes_for_thread_switch(history, model_label);
    // The rewound thread continues under a NEW file — keeping the old slug would make the
    // next autosave overwrite the backup that was just written.
    set_session_slug(None);
    update_live_history(history);
    println!(
        "{} #{} — files and conversation rewound",
        style("⏪ restored").color256(splash::ACCENT),
        snap.id
    );
    println!(
        "{}",
        style(format!(
            "  (your previous chat was saved as “{backup}” — /sessions to get it back)"
        ))
        .dim()
    );
    Ok(())
}

/// Rewind only the working tree to checkpoint `id` (reversible — pre-restore tree auto-saved).
fn files_restore(id: u32) -> Result<()> {
    let s = timemachine::restore(id).with_context(|| format!("restoring checkpoint #{id}"))?;
    println!(
        "{} #{} — files rewound; your chat is untouched",
        style("⏪ restored").color256(splash::ACCENT),
        s.id
    );
    println!(
        "{}",
        style("  (reversible — the pre-restore tree was auto-saved; pick it to go back)").dim()
    );
    Ok(())
}

// ───────────────────────────── discord bot daemon + setup ─────────────────────────────

async fn run_discord(cmd: DiscordCmd) -> Result<()> {
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

async fn run_telegram(cmd: TelegramCmd) -> Result<()> {
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
async fn apps_menu() -> Result<()> {
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
async fn skills_menu() -> Result<()> {
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
fn persona_self_view_n(slug: &str, name: &str, all: bool) {
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
async fn personas_menu(history: &mut Vec<Message>, model: &str) -> Result<()> {
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
async fn telegram_menu() -> Result<()> {
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

// ───────────────────────────── interactive landing menu ─────────────────────────────
// Bare `ng` (no subcommand) drops into a colored, arrow-key TUI (dialoguer): a status banner +
// a Select list (Setup / Chat / Agent / Models / Memory / Quit). ↑/↓ + Enter to choose, Esc to
// quit. This is the "open the CLI and see a UI" surface — every action also has a scriptable
// subcommand, so automation never depends on the menu. Needs a TTY; piped/CI prints a hint.

/// A one-accent (gold, matching the splash) theme — dim for secondary, bold for the active row.
/// One cohesive hue (no rainbow), like hermes.
pub(crate) fn ui_theme() -> ColorfulTheme {
    let gold = || Style::new().for_stderr().color256(splash::ACCENT);
    ColorfulTheme {
        prompt_prefix: style(String::new()).for_stderr(),
        prompt_suffix: style("›".to_string()).for_stderr().dim(),
        success_prefix: style("·".to_string()).for_stderr().dim(),
        success_suffix: style(String::new()).for_stderr(),
        error_prefix: style("✗".to_string()).for_stderr().red(),
        prompt_style: Style::new().for_stderr().bold(),
        values_style: gold(),
        hint_style: Style::new().for_stderr().dim(),
        active_item_style: gold().bold(),
        inactive_item_style: Style::new().for_stderr(),
        active_item_prefix: style("❯".to_string())
            .for_stderr()
            .color256(splash::ACCENT)
            .bold(),
        inactive_item_prefix: style(" ".to_string()).for_stderr(),
        ..ColorfulTheme::default()
    }
}

/// A bordered single-line input box (the "chat box") read key-by-key via `console` (raw mode), so
/// the box redraws as you type and the cursor sits inside it. A small line editor:
/// - type / **Backspace** / **Del** insert+delete at the cursor; **←/→** move; **Home/End** jump;
/// - **↑/↓** walk `history` (most-recent first; ↓ past the newest restores your in-progress draft);
/// - **Enter** submits; **Esc** clears the line AND any attached images (quits only when both are
///   already empty); **Ctrl-C/Ctrl-D** quit.
/// - **Attach an image** (vision) two ways (Ctrl-V can't be used — Windows Terminal eats it):
///   **Ctrl-O** grabs a copied screenshot from the clipboard (Win+Shift+S), or **drag an image file
///   onto the window** (the terminal pastes its path; the caller turns image-file paths on the line
///   into attachments on Enter). An `[N img]` tag shows in the top border; **Ctrl-X** removes the
///   most recent attachment (keeps your text).
///
/// Returns `Some((line, images))` on Enter (`images` = `data:` URLs of clipboard attachments; the
/// caller adds any file-path attachments), or `None` to quit (Esc-empty / Ctrl-C/D / EOF / non-TTY).
/// The visible window scrolls horizontally so the cursor stays in view on long lines.
fn read_input_box(history: &[String]) -> Result<Option<(String, Vec<String>)>> {
    use console::{Key, Term};
    use std::io::Write;
    const W: usize = 66; // inner width between the │ borders
    let text_cols = W - 3; // columns for editable text (after " ❯ ")

    let term = Term::stdout();
    let accent = splash::ACCENT;
    let bar = |l: &str, r: &str| {
        style(format!("{l}{}{r}", "─".repeat(W)))
            .color256(accent)
            .to_string()
    };
    // A small status tag in the TOP border (`╭───────[1 img]─╮`). ASCII-only + right-aligned, so the
    // width is exact and the border never tears (an emoji caption mis-measures by a cell). Empty tag
    // → a plain border.
    let top_bar = |tag: &str| -> String {
        if tag.is_empty() {
            return bar("╭", "╮");
        }
        let t = format!("[{tag}]");
        let fill = W.saturating_sub(t.chars().count() + 1);
        style(format!("╭{}{t}─╮", "─".repeat(fill)))
            .color256(accent)
            .to_string()
    };
    // Attachment count → tag text (empty when none, so the border goes plain).
    let count_tag = |n: usize| -> String {
        if n == 0 {
            String::new()
        } else {
            format!("{n} img")
        }
    };

    // Render the middle line for (chars, cursor), scrolling so the cursor is visible. Returns the
    // line + how far left to shift the cursor from the line end to land on `cursor`. (Char widths
    // are treated as 1 — fine for ASCII/Latin/Vietnamese; exotic wide input may wobble by a cell.)
    let render = |chars: &[char], cursor: usize, scroll: &mut usize| -> (String, usize) {
        if cursor < *scroll {
            *scroll = cursor;
        }
        if cursor >= *scroll + text_cols {
            *scroll = cursor + 1 - text_cols;
        }
        let end = (*scroll + text_cols).min(chars.len());
        let shown: String = chars[*scroll..end].iter().collect();
        let shown_w = end - *scroll;
        let pad = text_cols - shown_w;
        let line = format!(
            "{l} {arrow} {shown}{sp}{l}",
            l = style("│").color256(accent),
            arrow = style("❯").color256(accent).bold(),
            sp = " ".repeat(pad)
        );
        let cursor_col = cursor - *scroll;
        let back = (shown_w - cursor_col) + pad + 1; // chars after cursor + pad + right border
        (line, back)
    };

    let mut scroll = 0usize;
    let (mid0, back0) = render(&[], 0, &mut scroll);
    println!("{}", top_bar(""));
    println!("{mid0}");
    print!("{}", bar("╰", "╯"));
    std::io::stdout().flush().ok();
    term.move_cursor_up(1).ok();

    let place = |line: &str, back: usize| {
        let _ = term.clear_line();
        let mut o = std::io::stdout();
        let _ = write!(o, "\r{line}");
        let _ = o.flush();
        let _ = term.move_cursor_left(back);
    };
    place(&mid0, back0);

    // Repaint the TOP border (cursor sits on the middle line) — used by the image attach/remove keys
    // to reflect the count tag, then return to the middle line (the loop's `place` restores the
    // cursor column).
    let redraw_top = |s: &str| {
        let _ = term.move_cursor_up(1);
        let _ = term.clear_line();
        let mut o = std::io::stdout();
        let _ = write!(o, "\r{s}");
        let _ = o.flush();
        let _ = term.move_cursor_down(1);
    };

    let mut chars: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut hist_idx: Option<usize> = None; // Some = currently browsing history
    let mut draft: Vec<char> = Vec::new(); // the in-progress line saved when entering history
    let mut images: Vec<String> = Vec::new(); // pasted vision attachments (data: URLs)

    loop {
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => return Ok(None),
        };
        match key {
            Key::Enter => {
                let text: String = chars.iter().collect();
                // Collapse the 3-line box into a single compact `> …` echo (nothing when empty), so
                // the scrollback reads as a clean transcript instead of a stack of empty boxes — AND
                // so the box's presence is the unambiguous "your turn to type" signal (no box +
                // spinner/⊙ traces = the agent is working).
                term.move_cursor_down(1).ok(); // → bottom border
                term.clear_line().ok();
                term.move_cursor_up(1).ok(); // → middle line
                term.clear_line().ok();
                term.move_cursor_up(1).ok(); // → top border
                term.clear_line().ok();
                print!("\r");
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    println!(
                        "{} {}",
                        style("❯").color256(accent).bold(),
                        style(trimmed).dim()
                    );
                } else if !images.is_empty() {
                    println!(
                        "{} {}",
                        style("❯").color256(accent).bold(),
                        style(format!("📎 {} image(s)", images.len())).dim()
                    );
                }
                std::io::stdout().flush().ok();
                return Ok(Some((text, images)));
            }
            Key::Char('\u{f}') => {
                // Ctrl-O: grab a copied screenshot from the clipboard (Win+Shift+S / "Copy image").
                // Explicit, so it works in Windows Terminal (which eats Ctrl-V but forwards Ctrl-O).
                let tag = match image_input::clipboard_image_data_url() {
                    Ok(Some(url)) => {
                        images.push(url);
                        count_tag(images.len())
                    }
                    Ok(None) => "no image".to_string(),
                    Err(_) => "clip error".to_string(),
                };
                redraw_top(&top_bar(&tag));
            }
            Key::Char('\u{18}') => {
                // Ctrl-X: remove the most recently attached image (keeps your typed text). The tag
                // reflects the new count (gone when the last one is removed); no-op when none.
                if images.pop().is_some() {
                    redraw_top(&top_bar(&count_tag(images.len())));
                }
            }
            Key::Escape => {
                // Nothing typed AND nothing attached → quit. Otherwise clear the line AND drop any
                // attached images (a quick way to start over / undo a wrong attachment).
                if chars.is_empty() && images.is_empty() {
                    term.move_cursor_down(1).ok();
                    println!();
                    return Ok(None);
                }
                chars.clear();
                cursor = 0;
                hist_idx = None;
                if !images.is_empty() {
                    images.clear();
                    redraw_top(&top_bar(""));
                }
            }
            Key::Char('\u{3}') | Key::Char('\u{4}') => {
                term.move_cursor_down(1).ok();
                println!();
                return Ok(None);
            }
            Key::Char(c) if c.is_control() => continue, // ignore stray control chars (no redraw)
            Key::Char(c) => {
                chars.insert(cursor, c);
                cursor += 1;
            }
            Key::Backspace => {
                if cursor > 0 {
                    chars.remove(cursor - 1);
                    cursor -= 1;
                }
            }
            Key::Del => {
                if cursor < chars.len() {
                    chars.remove(cursor);
                }
            }
            Key::ArrowLeft => cursor = cursor.saturating_sub(1),
            Key::ArrowRight => {
                if cursor < chars.len() {
                    cursor += 1;
                }
            }
            Key::Home => cursor = 0,
            Key::End => cursor = chars.len(),
            Key::ArrowUp => {
                if history.is_empty() {
                    continue;
                }
                let next = match hist_idx {
                    None => {
                        draft = chars.clone(); // save the in-progress line
                        history.len() - 1
                    }
                    Some(0) => continue, // already at the oldest
                    Some(i) => i - 1,
                };
                hist_idx = Some(next);
                chars = history[next].chars().collect();
                cursor = chars.len();
            }
            Key::ArrowDown => match hist_idx {
                None => continue,
                Some(i) if i + 1 < history.len() => {
                    hist_idx = Some(i + 1);
                    chars = history[i + 1].chars().collect();
                    cursor = chars.len();
                }
                Some(_) => {
                    hist_idx = None; // past the newest → restore the draft
                    chars = draft.clone();
                    cursor = chars.len();
                }
            },
            _ => continue, // unhandled key → no redraw
        }
        let (m, b) = render(&chars, cursor, &mut scroll);
        place(&m, b);
    }
}

/// The interactive surface (bare `aizen`). Dispatches to the **sticky TUI** (pinned bottom input box +
/// continuous chat queue + Esc-to-cancel) on a real terminal, or the plain line-REPL fallback
/// (non-TTY-forced, or `AIZEN_NO_STICKY=1`). Needs a TTY; piped/CI prints a hint.
async fn run_menu() -> Result<()> {
    use std::io::IsTerminal;
    let forced = cli_config::branded_flag("MENU");
    if !forced && !std::io::stdin().is_terminal() {
        println!("aizen — Aizen agentic CLI");
        println!(
            "Run `aizen --help` for commands, or `aizen config` to set up the endpoint + key."
        );
        return Ok(());
    }
    icons::set_tier(cli_config::load().icons.as_deref()); // apply the persisted icon style
                                                          // First launch on a fresh install → a one-time welcome intro + guided setup, before the chat TUI.
    if needs_onboarding() {
        first_run_onboarding().await;
        icons::set_tier(cli_config::load().icons.as_deref()); // setup may have changed the icon style
    }
    // If the repo ships project-local MCP servers we haven't decided on, ask once (supply-chain gate).
    if let Some(n) = crate::agent::mcp::project_trust_prompt() {
        prompt_mcp_trust(n);
    }
    let sticky = std::io::stdout().is_terminal() && !cli_config::branded_flag("NO_STICKY");
    if sticky {
        run_menu_sticky().await
    } else {
        run_menu_plain().await
    }
}

/// One-time prompt when a cloned repo ships project-local MCP servers (`./.aizen/mcp.json`): trust
/// + load them, or dismiss (won't nag again — `aizen mcp trust` re-enables). MCP servers can run
/// commands, hence the explicit gate before auto-arming a stranger's repo.
fn prompt_mcp_trust(server_count: usize) {
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
fn needs_onboarding() -> bool {
    if cli_config::branded_flag("ONBOARD") {
        return true;
    }
    cli_config::load().onboarded != Some(true) && !endpoint_ready()
}

/// First-run experience for a freshly-downloaded `ng`: a branded welcome, then the setup wizard, then
/// an optional messaging-app connect — finally dropping into the normal chat TUI. Marks `onboarded`
/// up front so it shows exactly once (even if the user Ctrl-C's mid-setup); `aizen config` reruns setup.
async fn first_run_onboarding() {
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

/// The HUD line, per the mockup: `model  ·  ~<used>/<max> tok  ·  <n> turns  ·  <mode>` with an
/// optional persona / todo / agents chip. The raw token + turn counts are back on the row (the
/// mockup shows them); the graphical context meter is still fed via `tui::set_ctx_permille` and the
/// retained backend's footer tints the mode/persona chips as it draws the row.
fn status_text(history: &[Message], model: &str) -> String {
    let toks = session_tokens(history);
    let (window, _) = resolve_ctx_window(model);
    // Feed the graphical context meter (per-mille for sub-1% resolution); the footer draws the bar.
    let permille = (toks as f64 / window as f64 * 1000.0)
        .round()
        .clamp(0.0, 1000.0) as u16;
    tui::set_ctx_permille(permille);
    // One "turn" = one user message that opened an exchange (system prompt at [0] is not a turn).
    let turns = history.iter().filter(|m| m.role == "user").count();
    let turns_chip = format!("  ·  {turns} turn{}", if turns == 1 { "" } else { "s" });
    let tok_chip = format!("  ·  ~{}/{} tok", fmt_k(toks), fmt_k(window));
    let approval = approval_mode();
    let mode = if cli_config::ultimate_enabled() {
        "  ·  ✦ ultimate"
    } else if approval == ApprovalMode::Yolo {
        "  ·  ⚡ yolo"
    } else if approval == ApprovalMode::Smart {
        "  ·  ◆ smart"
    } else {
        ""
    };
    // Active persona chip — so it's always visible WHICH character aizen is role-playing (not just a
    // one-off "now playing" line that scrolls away). `🎭 Name`, styled by the footer's chip pass.
    let persona = cli_config::load()
        .persona
        .map(|p| format!("  ·  🎭 {p}"))
        .unwrap_or_default();
    let todos = crate::agent::todo::status_summary()
        .map(|s| format!("  ·  {s}"))
        .unwrap_or_default();
    let agents = crate::agent::orchestration::hud_chip()
        .map(|s| format!("  ·  {s}"))
        .unwrap_or_default();
    format!("{model}{tok_chip}{turns_chip}{persona}{mode}{todos}{agents}")
}

/// The summarizer endpoint: `roles.summarizer` routing (env > config > main endpoint). Chore
/// calls (compaction/handoff summaries) are the classic cheap-model candidates — one config field
/// and every summary routes there.
fn summarizer_endpoint(base: &str, key: &str, model: &str) -> cli_config::ResolvedEndpoint {
    cli_config::resolve_role(
        "summarizer",
        &cli_config::ResolvedEndpoint {
            base_url: base.to_string(),
            api_key: key.to_string(),
            model: model.to_string(),
        },
    )
}

/// Eager tool execution during streaming: ON unless disabled by config (`eager_tools: false`) or
/// the `AIZEN_NO_EAGER` env kill-switch (per-machine escape hatch if a provider's stream framing
/// misbehaves).
fn eager_enabled() -> bool {
    if cli_config::branded_flag("NO_EAGER") {
        return false;
    }
    cli_config::load().eager_tools.unwrap_or(true)
}

/// Live prompt-cache hit rate of the MOST RECENT model call (`⛁ 78% cached`), when the provider
/// reports usage and any tokens actually came from cache. The at-a-glance KV-cache health signal —
/// a sudden drop to 0% mid-session means something is rewriting the prefix.
fn cache_hit_label() -> Option<String> {
    let (prompt, cached, _) = client::cost_meter().last_call()?;
    if prompt == 0 || cached == 0 {
        return None;
    }
    Some(format!("⛁ {}% cached", cached * 100 / prompt))
}

/// Disarms the interactive cancel token AND resets working state however a turn ends — normal
/// completion, an early `continue` from a prep failure, or a panic unwinding out of the arm.
/// `disarm_cancel` is identity-checked, so this can never clear a NEWER turn's token and
/// double-disarming is harmless. `set_working(false)` is idempotent so a second reset is safe.
///
/// This exists because the token AND working indicator are now armed BEFORE the turn's prep work
/// (see the Chat arm), and an armed token is what `tui::turn_in_flight` reports. Leaking one past a
/// `continue` would leave the REPL idle while Esc still behaved like "cancel" and the UI showed
/// "working", so every exit path must disarm AND reset.
struct TurnCancelGuard(crate::core::cancel::TurnCancel);

impl Drop for TurnCancelGuard {
    fn drop(&mut self) {
        tui::set_working(false);
        tui::disarm_cancel(&self.0);
    }
}

/// Closes the steering mailbox on every exit path, re-queueing anything the turn never picked up.
///
/// Pairs with [`TurnCancelGuard`], and for the same reason: the mailbox is now armed BEFORE the turn's
/// prep (so a steer typed during retrieval reaches the run instead of the queue), and prep has several
/// early `continue`s — an unconfigured endpoint, a `#remember`/`!shell` input, an Esc during prep. A
/// mailbox left armed past one of those would accept steers into a slot nothing drains: the input
/// thread would see `is_armed()` and hand over a message that then sat there until some later turn
/// armed it again and `arm()`'s clear discarded it. Silently eating user input is worse than queueing
/// it, which is what makes this guard the thing that licenses arming early at all.
///
/// Leftovers come back as ordinary submissions so a steer typed in the last instants of a turn runs as
/// the next one rather than vanishing. Idempotent: the normal end-of-turn path disarms explicitly, so
/// this fires on an already-empty, already-closed mailbox and does nothing.
struct SteerMailboxGuard(tokio::sync::mpsc::UnboundedSender<tui::Submission>);

impl Drop for SteerMailboxGuard {
    fn drop(&mut self) {
        for leftover in crate::core::steer::disarm() {
            if self
                .0
                .send(tui::Submission::Chat(leftover, Vec::new()))
                .is_ok()
            {
                tui::note_submission_enqueued();
            }
        }
    }
}

/// Run a slash command's network call as INTERRUPTIBLE work. `None` means the user pressed Esc.
///
/// Slash handlers that call the model (`/compact`, `/handoff`) used to `await` straight inside the
/// REPL loop with no token armed and `WORKING` still false. Two consequences, both bad: the HTTP
/// client's 300s read timeout became the real ceiling, and `tui::turn_in_flight()` reported false —
/// so Esc took the idle branch and merely cleared the draft while the REPL sat blocked in the await,
/// consuming no submissions. A slow or hung endpoint therefore froze the whole app for up to five
/// minutes with no spinner and no way out. This is the confirmed "/compact makes it hang".
///
/// Arming the token is what makes Esc live (the input thread's `request_cancel` cancels exactly this
/// token); `set_working` puts the pill up so the wait is visibly work. The guard disarms on every
/// exit path including a panic, and dropping the future at its await point aborts the request.
async fn cancellable_slash<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    let token = crate::core::cancel::TurnCancel::new();
    tui::arm_cancel(token.clone());
    let _guard = TurnCancelGuard(token.clone());
    tui::set_working(true);
    let out = crate::core::cancel::race(&token, fut).await;
    tui::set_working(false);
    out
}

/// Like [`cancellable_slash`] but shows a specific caption in the working pill instead of the
/// default whimsical verb — so post-turn housekeeping ("learning from this turn…") is visually
/// distinct from the main agent turn ("Pondering…"), and the user knows Esc will skip optional
/// work, not abort the answer they already received.
async fn cancellable_slash_labeled<T>(
    label: &str,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    let token = crate::core::cancel::TurnCancel::new();
    tui::arm_cancel(token.clone());
    let _guard = TurnCancelGuard(token.clone());
    // Set working BEFORE caption: Working(true) seeds a random verb and calls set_work_caption
    // internally, so sending WorkCaption first would be overwritten. Sending WorkCaption AFTER
    // Working(true) replaces the verb with the intended label.
    tui::set_working(true);
    tui::set_work_caption(label);
    let out = crate::core::cancel::race(&token, fut).await;
    tui::set_working(false);
    out
}

/// The startup identity banner: one line saying which project root + zone slug THIS launch is
/// bound to (the audit's top visibility gap: no surface printed either), plus loud notes when git
/// resolved unusually or a legacy zone from the old slug keying still holds data.
fn identity_banner() -> (String, Vec<String>) {
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
fn run_id_migration_once() {
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
fn startup_update_probe() {
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
fn resume_hint() -> Option<String> {
    let (name, n, origin) = most_recent_session()?;
    let origin_note = origin.map(|o| format!(", {o}")).unwrap_or_default();
    Some(format!(
        "⟲ last conversation “{}” ({n} messages{origin_note}) — /resume to continue it",
        pretty_session_name(&name)
    ))
}

/// Pull drag-dropped / pasted image paths out of a typed line, leaving the prose behind.
///
/// The other half of Ctrl-O clipboard attach: only real image files are lifted, so a message that
/// merely mentions a path keeps its text intact.
fn lift_image_attachments(line: &mut String, images: &mut Vec<String>) {
    if line.is_empty() {
        return;
    }
    let (cleaned, from_line) = image_input::extract_image_attachments(line);
    if !from_line.is_empty() {
        images.extend(from_line);
        *line = cleaned;
    }
}

/// Build this turn's tool registry against the resolved endpoint.
fn build_turn_registry(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
) -> Result<agent::tools::ToolRegistry> {
    agent::builtin::default_registry_with_task(
        http.clone(),
        ep.base_url.clone(),
        ep.api_key.clone(),
        ep.model.clone(),
        approval_mode(),
        resolve_ctx_window(&ep.model).0,
        None, // cwd IS the project in the REPL
    )
}

/// The agent config for one interactive turn.
///
/// `enable_steering` is the only thing the two surfaces disagree on: the retained REPL has a
/// mailbox the user can type into mid-turn, the plain one does not. Everything else — approval
/// mode, context window, self-review, LSP state, goal mode, the mid-turn snapshot — is a reading of
/// live config that must not be allowed to differ between them, which is why it is written once.
fn turn_agent_config(
    cancel: crate::core::cancel::TurnCancel,
    model: &str,
    enable_steering: bool,
) -> AgentConfig {
    AgentConfig {
        approval_mode: approval_mode(),
        cancel,
        context_window: resolve_ctx_window(model).0,
        enable_self_review: cli_config::self_review_enabled(&cli_config::load()),
        // Reflect the live manager state (honours `/lsp off` for this turn).
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        // Goal mode (set by `/goal <text>`): threads the live goal into this turn so the loop runs
        // cap-free with smart retry until the goal is declared and verified.
        goal: crate::agent::goal::current_goal(),
        // Only the interactive top-level turn reads the steering mailbox — a course correction the
        // user typed is aimed at THIS task, not at whatever a delegated sub-agent is doing.
        enable_steering,
        // Keep the exit-flush snapshot current DURING the turn, not just at its edges.
        on_progress: Some(publish_live_history),
        // The user is sitting here waiting: retry a 429/5xx blip many times (like the Claude CLI)
        // with FAST backoff (`interactive_backoff_ms`), so the whole chain still fits in ~30–40s and
        // then reports a clear error. 10 is a CEILING, not a cost — a gateway that recovers on try 2
        // stops at 2. `/goal` does not take this branch (goal mode retries transient errors
        // indefinitely, see agent/mod.rs), so raising this never shortens a goal run.
        max_transient_retries: 10,
        ..AgentConfig::default()
    }
}

/// Fold this turn's retrieved context into the outgoing message and seat it in history.
///
/// `line` stays the clean text the user typed — the fold only affects what is SENT, so the
/// checkpoint, the display and the persisted transcript all keep the original. The dynamic prompt
/// lane is refreshed AFTER the fold, never before: recall seats this turn's handle→id ledger and the
/// `<skills>` lane ranks itself by affinity to exactly those facts, so refreshing first would build
/// the index against the PREVIOUS turn's recall.
fn seat_user_message(line: &str, images: Vec<String>, history: &mut Vec<Message>, model: &str) {
    let sent = fold_context_into_query(line);
    refresh_dynamic_prompt_lane(history, model);
    if images.is_empty() {
        history.push(Message::user(sent));
    } else {
        tui::emit_line(
            &style(format!("📎 {} image(s) attached", images.len()))
                .color256(splash::ACCENT)
                .to_string(),
        );
        history.push(Message::user_with_images(sent, images));
    }
    // Refresh the exit-flush snapshot the moment the user turn lands, so an abrupt window close
    // mid-turn still persists the question (the per-turn autosave only runs on success).
    update_live_history(history);
}

// ── the shared turn: everything both REPL surfaces do identically ─────────────────────────────
// `run_menu_sticky` and `run_menu_plain` differ in exactly two things — how a line arrives, and
// whether Esc can race the model call. Everything else about a turn is the same work, and it used
// to be written out twice. It drifted: the plain loop once ran the skill, persona and memory passes
// in a different order than the retained one, and to this day it was the copy that never learned
// about goal-mode completion, the post-turn timeout ceiling, or the recovery checkpoints. The two
// functions below are the parts that were always meant to be one thing.
//
// They are surface-agnostic because `tui::emit_line` already is: it renders through the retained
// backend when one is running and prints append-only when none is, so the same call reads correctly
// on both. Nothing here may use `println!` — see `tui::note_line`.

/// Run one agent turn against `ep`, with the model wiring both REPLs were building by hand.
///
/// The three closures (stream the turn, summarize for mid-loop compaction, optionally consult the
/// `oracle` role for self-review) are pure functions of the endpoint, so there was never a reason
/// for two copies. The caller keeps ownership of cancellation: the retained REPL races this future
/// against Esc, the plain one simply awaits it.
async fn run_agent_turn(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
    cfg: &AgentConfig,
    registry: &agent::tools::ToolRegistry,
    history: &mut Vec<Message>,
) -> Result<AgentOutcome> {
    let base = ep.base_url.as_str();
    let key = ep.api_key.as_str();
    let model = ep.model.as_str();
    let eager_on = eager_enabled();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        if eager_on {
            // Read-only calls start the moment their streamed args complete.
            let starter = agent::eager_starter(registry, cfg);
            client::stream_chat_with_tools_eager(
                http,
                base,
                key,
                model,
                &msgs,
                &defs,
                Some(&starter),
            )
            .await
        } else {
            client::stream_chat_with_tools(http, base, key, model, &msgs, &defs).await
        }
    };
    // Non-streaming summarizer for mid-loop auto-compaction (keeps the streamed display clean).
    let sum_ep = summarizer_endpoint(base, key, model);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    // Optional oracle for self-review: only when `roles.oracle` names a stronger reviewer model;
    // otherwise the loop falls back to nudge-mode.
    let oracle = cli_config::role_configured("oracle")
        .then(|| cli_config::resolve_role("oracle", ep))
        .map(|role| {
            move |msgs: Vec<Message>| {
                let role = role.clone();
                async move {
                    chore_chat(http, &role.base_url, &role.api_key, &role.model, &msgs, &[])
                        .await
                        .map(|t| t.content.unwrap_or_default())
                }
            }
        });
    agent::run_agent_loop_full(chat, summarize, oracle, cfg, registry, history).await
}

/// How long the post-turn learning passes may take in total before the REPL gives up on them.
///
/// Each call already has its own 300s ceiling (`chore_chat` → `subagent_call_timeout`), but three of
/// them in a row can strand an idle-looking REPL for fifteen minutes. On timeout the user sees a
/// skip line instead of a spinner that never stops.
const POST_TURN_OVERALL_TIMEOUT_SECS: u64 = 600;

/// Everything a turn that reached the model must do afterwards, on either surface.
///
/// Ordering is load-bearing and was the thing that drifted: the learning passes read the FULL detail
/// of the turn, so they must run before auto-compaction summarizes it away, and persistence must
/// happen last and unconditionally — a cancelled learning pass is not a reason to lose the
/// transcript.
async fn finish_turn(
    outcome: &AgentOutcome,
    persona_before: Option<String>,
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
) {
    // ABNORMAL STOP, SAID OUT LOUD. The loop can end for reasons that are NOT success — the repair
    // budget ran out with the tree still broken, the step cap was hit mid-task, the model started
    // repeating itself — and in each case it has usually already streamed a confident closing
    // paragraph. Silence here makes those read exactly like `Done`, and the passes below would then
    // file a red tree as a finished task.
    surface_abnormal_stop(outcome);
    // Goal mode finishes only on a verify-passing `Done`. Clear it here so the next turn is an
    // ordinary capped turn again; Esc leaves the goal armed on purpose, so the user can retry.
    if crate::agent::goal::current_goal().is_some() && matches!(outcome.stop, StopReason::Done) {
        crate::agent::goal::set_goal(None);
        crate::agent::goal::arm(false);
        crate::agent::goal::clear();
        tui::emit_line(
            &style("🎯 goal complete — verified. goal mode off.")
                .color256(splash::ACCENT)
                .to_string(),
        );
    }
    // An EMPTY answer from a SINGLE model call (no tool work, no streamed text) used to vanish
    // silently — a rate limit swallowed into an empty 200, a content filter, or a gateway that
    // streams `[DONE]` with no deltas looked identical to "still idle". `iters <= 1` keeps a turn
    // that DID do tool work and merely ended without a closing sentence from being flagged.
    let empty = outcome
        .final_text
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();
    if empty && outcome.iters <= 1 {
        tui::emit_line(&format!(
            "{} the model returned an empty response — no text and no tool calls. Likely a rate limit, content filter, or a gateway that closed the stream early. Try again, or /model to switch.",
            theme::warn("⚠ empty reply:")
        ));
    }
    // The agent may have created or switched personas mid-turn (the `persona_create` tool). Resync
    // the system prompt at the turn boundary so the new character is live from the next message —
    // prefix-cache safe, because index 1 is rewritten between turns rather than during one.
    let persona_after = cli_config::load().persona;
    if persona_after != persona_before {
        update_system_prompt(history, &ep.model);
        if let Some(name) = persona_after {
            tui::emit_line(
                &style(format!("🎭 now playing: {name} (from your next message)"))
                    .color256(splash::ACCENT)
                    .to_string(),
            );
        }
    }
    // The learning passes are model calls made after the turn's own token was disarmed, so without
    // re-arming, Esc would take the idle branch while the REPL sat awaiting them: to the user the
    // turn had visibly ended and the app was wedged anyway. Cancelling here skips the remaining
    // learning, which is always optional work.
    let learning = cancellable_slash_labeled("learning from this turn…", async {
        maybe_run_secretary(history, http, &ep.base_url, &ep.api_key, &ep.model).await;
        maybe_evolve_persona(http, &ep.base_url, &ep.api_key, &ep.model).await;
        maybe_auto_compact(history, http, &ep.base_url, &ep.api_key, &ep.model).await;
    });
    let learned = match tokio::time::timeout(
        std::time::Duration::from_secs(POST_TURN_OVERALL_TIMEOUT_SECS),
        learning,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tui::emit_line(
                &theme::muted("⏱ post-turn learning exceeded timeout — skipped.").to_string(),
            );
            None
        }
    };
    if learned.is_none() {
        tui::emit_line(&theme::muted("⏹ skipped the post-turn learning passes.").to_string());
    }
    // Persistence is NOT optional, so it sits outside that block: a cancelled learning pass must
    // still leave the conversation on disk. `autosave_session` names the session with a model call,
    // so it is cancellable too — the local-only writer keeps the transcript either way.
    if cancellable_slash(autosave_session(
        history,
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
    ))
    .await
    .is_none()
    {
        autosave_last(history, Some(&ep.model));
    }
}

/// The sticky-TUI REPL: a background keyboard thread feeds a submission queue while the agent runs,
/// the input box stays pinned at the bottom, and Esc/Ctrl-C cancels an in-flight turn.
async fn run_menu_sticky() -> Result<()> {
    let http = http_client()?;
    let mut model_label = cli_config::load()
        .model
        .unwrap_or_else(|| "(no model)".to_string());
    // Pin this window to the model it launched with, so another window's `/model` (which rewrites the
    // shared cli-config.json) can't retarget this one on its next turn. Skipped when no model is set
    // yet — a first-run window must still adopt whatever `/config`/`/model` configures here.
    cli_config::pin_session_model(&model_label);
    let mut history: Vec<Message> = Vec::new();
    rebuild_system(&mut history, &model_label);
    let repo_scope = crate::core::recovery::current_repo_scope();

    // Text-only splash: the retained alt-screen renderer sanitizes CSI and would pass a raw sixel DCS
    // image through as garbage, so the intro is a Braille sun → pure printable text.
    let intro = format!(
        "{}\n{}",
        splash::render_text_only(),
        style("Type to talk — messages queue while it works · Esc cancels a running turn · /help · /quit")
            .dim()
    );
    // The retained backend is the only interactive surface. If it can't take the terminal (alt-screen
    // refused), there is no second renderer to degrade into — hand off to the plain line-REPL rather
    // than run this loop headless, which would queue keystrokes against a UI that never painted.
    if !tui::activate(&intro, &status_text(&history, &model_label)) {
        return run_menu_plain().await;
    }
    tui::set_ultimate(cli_config::ultimate_enabled()); // open the input box in the right colour (gold if ultimate)
    install_exit_flush_handler(); // flush the live chat if the terminal window is closed (Windows ✕)
    {
        let (main, notes) = identity_banner();
        tui::emit_line(&style(main).dim().to_string());
        for n in notes {
            tui::emit_line(&style(n).color256(theme::WARN).to_string());
        }
        startup_update_probe();
    }
    crate::core::recovery::begin(repo_scope.clone(), current_session_slug());
    // Housekeeping for everything the previous runs could not clean up after themselves: lease
    // directories from abrupt shutdowns (the clean-exit path never ran) and staging files from a
    // write that was killed mid-rename. Both are pure accumulation — neither affects correctness,
    // and neither had any collector before.
    crate::core::recovery::sweep_expired();
    sweep_orphan_temps();
    // Publish this window to the repository's session registry so the OTHER aizen windows (and the
    // one that eventually reviews and commits) can see it exists, what it is doing, and which files
    // it has changed. Best-effort: a registry failure never blocks the REPL.
    coop::begin(current_session_slug());
    if let Some(line) = coop::peers_banner() {
        tui::emit_line(&style(line).dim().to_string());
    }
    if let Some(offer) = crate::core::recovery::scan_stale(&repo_scope)
        .into_iter()
        .next()
    {
        tui::emit_line(
            &style(format!("⟳ {}", crate::core::recovery::format_offer(&offer)))
                .dim()
                .to_string(),
        );
        tui::emit_line(
            &style("  /recover restore · /recover discard")
                .dim()
                .to_string(),
        );
    } else if let Some(hint) = resume_hint() {
        // Suppressed when a crash-recovery offer is showing: two competing restore prompts in a
        // row is worse than one, and `/recover` (which carries an unsent draft + checkpoint id)
        // wins.
        tui::emit_line(&style(hint).dim().to_string());
    }
    // Background model health poller: colours the idle `● ready` chip green/yellow/red from a real
    // GET /models probe every 60s (plus once immediately). Independent of the chat HTTP client so a
    // long-running turn's keep-alive doesn't share the short health timeout.
    spawn_health_poller();
    spawn_reconcile_pass();
    let mut input = tui::spawn_input();
    // Off-to-the-side Q&A: a long-lived worker answers `?`-prefixed questions on its OWN thread,
    // reading a read-only snapshot of the live conversation, WITHOUT touching the turn in flight
    // (no history mutation, no cancel, no WORKING). See `core::aside`.
    spawn_aside_worker(http.clone());

    loop {
        let sub = match input.submissions.recv().await {
            Some(s) => s,
            None => break,
        };
        tui::note_submission_dequeued();
        match sub {
            tui::Submission::Quit => break,
            tui::Submission::Slash(cmd) => {
                if cmd.trim().is_empty() || slash_is_interactive(&cmd) {
                    // Dialoguer menus / long-running daemons drive the terminal directly → suspend
                    // the sticky box, run, then re-enter.
                    tui::suspend();
                    let outcome = if cmd.trim().is_empty() {
                        slash_menu(&mut history, &mut model_label).await
                    } else {
                        handle_slash(&cmd, &mut history, &mut model_label).await
                    };
                    icons::set_tier(cli_config::load().icons.as_deref());
                    if matches!(outcome, SlashOutcome::Quit) {
                        let _ = input.resume.send(());
                        break;
                    }
                    tui::resume(&status_text(&history, &model_label));
                    let _ = input.resume.send(()); // unpark the keyboard thread
                                                   // A custom command expanded to a prompt → re-inject it as a chat submission so the
                                                   // next loop iteration runs it through the normal agent path (with cancel support).
                    if let SlashOutcome::Submit(prompt) = outcome {
                        let _ = input.inject.send(tui::Submission::Chat(prompt, Vec::new()));
                    }
                } else {
                    // Pure-print command: keep the sticky box up. The handler's `tui::emit_line`
                    // output flows into the scroll region ABOVE the box, so short output (/mcp,
                    // /cost, /tokens, /yolo …) is preserved instead of being painted over by the
                    // box on resume (the "/mcp shows nothing" bug).
                    let outcome = handle_slash(&cmd, &mut history, &mut model_label).await;
                    icons::set_tier(cli_config::load().icons.as_deref());
                    let _ = input.resume.send(()); // unpark the keyboard thread
                    if matches!(outcome, SlashOutcome::Quit) {
                        break;
                    }
                    tui::set_status(&status_text(&history, &model_label));
                    if let SlashOutcome::Submit(prompt) = outcome {
                        let _ = input.inject.send(tui::Submission::Chat(prompt, Vec::new()));
                    }
                }
            }
            tui::Submission::Chat(line, images) => {
                let mut line = line.trim().to_string();
                let mut images = images;
                lift_image_attachments(&mut line, &mut images);
                if line.is_empty() && images.is_empty() {
                    continue;
                }
                // SET WORKING EARLY — user just submitted, show the indicator immediately so typing
                // feels responsive. The prep work below (codebase RAG, LSP, registry) is real and
                // visible latency; showing "working" throughout it is honest UX.
                tui::set_working(true);
                // ARM CANCEL FIRST — before any prep. Everything between here and the agent loop
                // is real latency the user can see and will try to interrupt: `@file` expansion, the
                // dynamic prompt-lane rebuild, codebase retrieval, the recovery checkpoint, LSP
                // spawn, registry construction. The token being armed is what makes `turn_in_flight`
                // true, so Esc cancels throughout that window instead of silently clearing the draft
                // while the turn starts anyway. The guard disarms on EVERY exit path (including the
                // `continue`s below), so an aborted prep never leaves Esc mis-wired.
                let turn_cancel = crate::core::cancel::TurnCancel::new();
                tui::arm_cancel(turn_cancel.clone());
                let _cancel_guard = TurnCancelGuard(turn_cancel.clone());
                // OPEN THE STEERING MAILBOX IN THE SAME BREATH, for the same window and the same
                // reason. This used to sit ~150 lines below, just before `set_working(true)`, which
                // made the two flags disagree for the whole prep span above: `turn_in_flight()` reads
                // the ARMED TOKEN and was already true, while `steer::is_armed()` was still false. The
                // input thread's `>` prefix (and Alt+Enter) asks the mailbox, so every steer typed
                // during prep was refused and fell through to the post-turn queue — the user sees the
                // turn starting, types `> also do X`, and watches it queue instead. Retrieval is the
                // slow part of prep, so the window was widest exactly on the big tasks worth steering.
                //
                // The guard is what makes arming this early safe: three `continue`s below abort prep,
                // and a mailbox left armed with no turn behind it would swallow steers into a slot
                // nothing drains.
                crate::core::steer::arm();
                let _steer_guard = SteerMailboxGuard(input.inject.clone());
                // Input-box affordances on a typed message (skipped for a vision message): `#remember`
                // / `!shell-escape` run no turn; a normal message has its `@file`·`` !`cmd` `` expanded.
                let echo_src = line.clone();
                if images.is_empty() {
                    match preprocess_input(&line) {
                        InputPre::Handled => continue,
                        InputPre::Send(expanded) => line = expanded,
                    }
                }
                // Echo the ORIGINAL typed text (not a big @file expansion) into the scrolling
                // transcript — the box was cleared on submit, so otherwise it wouldn't show.
                let echo = if echo_src.is_empty() {
                    "(image)".to_string()
                } else {
                    echo_src.clone()
                };
                // Colour the WHOLE echoed line (arrow + text) in the moonlight accent, not just the
                // `❯` glyph — so a user turn reads as one distinct block against the model's grey
                // reply. In the retained TUI the SGR now survives (see `ansi_spans`); classic prints
                // it directly. The arrow stays bold as the turn anchor.
                tui::emit_line(&format!(
                    "{} {}",
                    style("❯").color256(splash::ACCENT).bold(),
                    style(&echo).color256(splash::ACCENT)
                ));
                let (base_url, api_key, model) = match resolve_endpoint(None, None, None) {
                    Ok(t) => t,
                    Err(_) => {
                        tui::emit_line(
                            &style("Not set up yet — /config (or /model to pick a model).")
                                .dim()
                                .to_string(),
                        );
                        continue;
                    }
                };
                // A quiet rotating tip under the message (Claude-Code style) — a discoverability
                // nudge that advances per turn. Empty when tips are off (`AIZEN_NO_TIPS`) or off-TTY.
                // Placed after the endpoint check so an unconfigured REPL doesn't burn a tip.
                let tip = tui::next_tip();
                if !tip.is_empty() {
                    tui::emit_line(
                        &style(format!("  {}{}", icons::g(icons::tip()), tip))
                            .dim()
                            .to_string(),
                    );
                }
                model_label = model.clone();
                // This turn's endpoint, resolved once and shared by everything downstream: query
                // expansion, the agent loop, and the post-turn passes all read the same triple.
                let ep = cli_config::ResolvedEndpoint {
                    base_url: base_url.clone(),
                    api_key: api_key.clone(),
                    model: model.clone(),
                };
                // Seed the query-expansion endpoint from THIS turn's resolution so a non-English
                // `codebase_search` can translate itself to English identifiers via the chore model.
                // Re-seeded every turn so it follows a mid-session `/model` switch.
                agent::query_lang::set_expansion_endpoint(&ep);
                migrate_legacy_prompt_lanes(&mut history, &model);
                // The rotating discoverability tip is emitted AFTER the turn finishes (see the
                // success branch below) so it lands UNDER the model's final answer, not stranded
                // above it at turn start.
                // Per-turn reasoning-effort auto-detect: classify what the user TYPED, not the
                // expanded payload. An `@file` may contain thousands of words, a code fence, or a
                // stray "quick"/"fast" in a comment; none of that says how hard THIS request is.
                // The expanded `line` still goes to the model unchanged — only routing reads the
                // clean source text.
                let effort_src = if echo_src.trim().is_empty() {
                    &line
                } else {
                    &echo_src
                };
                let eff = resolve_turn_effort(effort_src);
                cli_config::set_effort_override(eff.clone());
                tui::emit_line(&effort_turn_line(eff.as_deref()));
                if let Err(e) = crate::core::recovery::checkpoint_history(
                    &history,
                    Some(&line),
                    crate::core::recovery::RecoveryPhase::WaitingModel,
                ) {
                    tui::emit_line(
                        &style(format!("recovery boundary unavailable for this turn: {e}"))
                            .dim()
                            .to_string(),
                    );
                }
                // Fold memory recall + codebase RAG into the SENT content (not the dynamic system
                // lane) so index 1 stays byte-stable and the transcript-tail prefix cache holds.
                // `line` itself is unchanged → checkpoint / display / persisted history keep the
                // clean user text.
                seat_user_message(&line, images, &mut history, &model);
                let persona_before = cli_config::load().persona;
                // Arm LSP BEFORE building the registry — tools only register while enabled.
                arm_lsp_session();
                let registry = match build_turn_registry(&http, &ep) {
                    Ok(r) => r,
                    Err(e) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        history.pop();
                        continue;
                    }
                };
                let cfg = turn_agent_config(turn_cancel.clone(), &model, true);

                // Esc pressed DURING prep already cancelled this token — honour it instead of firing
                // the request anyway. Without this, cancelling in the prep window (the very thing the
                // early arm above made possible) would still send the turn to the model.
                if turn_cancel.is_cancelled() {
                    tui::emit_line(&theme::muted("⏹ stopped.").to_string());
                    history.pop(); // drop the user message this turn never ran
                    while input.cancel.try_recv().is_ok() {}
                    cli_config::clear_effort_override();
                    continue;
                }
                // (The steering mailbox was armed with the cancel token, before prep — see there.)
                while input.cancel.try_recv().is_ok() {} // drain any stale wake-up
                                                         // NOTE: the "✦ Pondering…" turn-start verb is now the bottom-of-transcript working
                                                         // line (see `working_line` in retained.rs) — no separate emit needed here.
                crate::core::recovery::set_phase(
                    crate::core::recovery::RecoveryPhase::WaitingModel,
                );
                // Tell the other windows this one is busy, and — unless the user pinned a task with
                // `/team task` — describe what with the prompt that started the turn.
                coop::suggest_task(&line);
                coop::set_state(coop::SessionState::Working);

                // Run the turn racing a cancel signal; on cancel the future is DROPPED at its current
                // await (model stream / tool batch / verify gate), which aborts the in-flight request.
                // History stays consistent under the drop because the loop PRE-FILLS: the assistant
                // tool-call turn and one placeholder result per call are appended in a single
                // synchronous block before any tool await, and real results overwrite the
                // placeholders as they land (see agent::execute_calls).
                // Run the turn racing a cancel signal; on cancel the future is DROPPED at its
                // current await (model stream / tool batch / verify gate), which aborts the
                // in-flight request. History stays consistent under the drop because the loop
                // PRE-FILLS: the assistant tool-call turn and one placeholder result per call are
                // appended in a single synchronous block before any tool await, and real results
                // overwrite the placeholders as they land (see agent::execute_calls).
                let result = {
                    let fut = run_agent_turn(&http, &ep, &cfg, &registry, &mut history);
                    tokio::select! {
                        r = fut => Some(r),
                        // Match only a REAL signal: if the keyboard thread exits (read_key error or
                        // EOF) its cancel_tx drops and recv() resolves to None — the `Some(())`
                        // pattern fails, tokio disables this branch, and the turn completes instead
                        // of being spuriously killed with "(interrupted by user)".
                        Some(()) = input.cancel.recv() => None,
                    }
                };
                tui::set_working(false);
                tui::disarm_cancel(&turn_cancel);
                // Close the steering mailbox HERE, on the normal path, so leftovers are re-injected
                // before `seal_turn` and in order. Anything typed in the last instants of the turn
                // (after the loop's final drain) comes back rather than vanishing. On Esc the `None`
                // arm below flushes the queue, which is the right call there: stop means stop.
                //
                // `_steer_guard` is the BACKSTOP, not a duplicate: it covers the prep-window
                // `continue`s and a panic, which never reach this line. `disarm` is idempotent
                // (second call yields nothing), so whichever runs first wins and the other is a no-op.
                for leftover in crate::core::steer::disarm() {
                    let _ = input
                        .inject
                        .send(tui::Submission::Chat(leftover, Vec::new()));
                    tui::note_submission_enqueued();
                }
                crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::Finalizing);
                // Attribute this turn's file changes to this session, before any other window's turn
                // can start writing. Every branch below (ok / clarify / interrupt / error) flows
                // through here, which is exactly the coverage the ledger needs: a cancelled turn that
                // already wrote three files must still be reviewable by the coordinator.
                for warning in coop::seal_turn() {
                    tui::emit_line(&style(warning).color256(theme::WARN).to_string());
                }
                // `_` bindings only, so this reads the result without moving it out of the match below.
                coop::set_state(if matches!(result, Some(Err(_))) {
                    coop::SessionState::Failed
                } else {
                    coop::SessionState::Idle
                });
                // Disarm the per-turn effort override the moment the turn ends — every branch below
                // (ok / clarify / interrupt / error) flows through here, so effort never leaks into
                // the next turn regardless of how this one finished.
                cli_config::clear_effort_override();

                match result {
                    None => {
                        tui::emit_line(&theme::muted("⏹ stopped.").to_string());
                        history.push(Message::assistant("(interrupted by user)".to_string()));
                        // Esc means "stop" — also clear any queued submissions (type-ahead backlog or
                        // a stray multi-line paste) so one Esc halts everything instead of the next
                        // queued turn auto-firing.
                        let mut flushed = 0usize;
                        while input.submissions.try_recv().is_ok() {
                            tui::note_submission_dequeued();
                            flushed += 1;
                        }
                        tui::clear_submission_depth();
                        if flushed > 0 {
                            tui::emit_line(
                                &theme::muted(format!("  cleared {flushed} queued message(s)."))
                                    .to_string(),
                            );
                        }
                        // Persist the cancelled turn. Only the success arm reaches
                        // `autosave_session`, so a turn stopped with Esc used to leave the session
                        // file at whatever the LAST successful turn wrote — every question and tool
                        // result from the cancelled run was lost on quit. Cancelling is not a reason
                        // to forget: the partial transcript is exactly what the user comes back to.
                        autosave_last(&history, Some(&model));
                    }
                    // `clarify` paused the turn awaiting the user's answer — show the question and
                    // loop back to the input box (the next message continues this conversation).
                    // Skip the post-turn learning/compaction passes: the turn isn't finished yet.
                    Some(Ok(AgentOutcome {
                        stop: StopReason::AwaitingInput(q),
                        ..
                    })) => {
                        show_clarify(&q);
                        // Same reason as the Esc arm: this branch deliberately skips the post-turn
                        // passes because the turn isn't finished, but the question the agent asked is
                        // real conversation. Persist it so quitting at the prompt doesn't drop it.
                        autosave_last(&history, Some(&model));
                    }
                    Some(Ok(outcome)) => {
                        // ABNORMAL STOP, SAID OUT LOUD. The loop can end for reasons that are NOT
                        // success — the repair budget ran out with the tree still broken, the step cap
                        // was hit mid-task, the model started repeating itself — and in every one of
                        // them the model has usually already streamed a confident closing paragraph.
                        // Without this the three read EXACTLY like `Done`: the post-turn passes below
                        // file the run as a normal episode and store it as a normal session, so a red
                        // tree is remembered as a finished task. The one-shot `aizen agent` path has
                        // reported these since it was written (see the `match outcome.stop` in
                        // `run_agent_cmd`); the REPL — where the user actually lives — never did.
                        finish_turn(&outcome, persona_before, &mut history, &http, &ep).await;
                    }
                    Some(Err(e)) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        if history.last().map(|m| m.role == "user").unwrap_or(false) {
                            history.pop();
                        }
                        // Persist the failed turn so quitting doesn't lose it
                        autosave_last(&history, Some(&model));
                    }
                }
                tui::set_status(&status_text(&history, &model_label));
                if let Err(e) = crate::core::recovery::checkpoint_history(
                    &history,
                    None,
                    crate::core::recovery::RecoveryPhase::Idle,
                ) {
                    tui::emit_line(
                        &style(format!("recovery checkpoint not updated: {e}"))
                            .dim()
                            .to_string(),
                    );
                }
            }
        }
    }
    // Flush the live conversation on graceful exit (/quit, Ctrl-D, Quit submission) so it's always in
    // /sessions — the per-turn autosave misses a turn that failed or was cancelled mid-flight.
    flush_live_session_on_exit();
    tui::deactivate();
    crate::core::recovery::clear();
    // Unlike recovery, the coop manifest SURVIVES a clean exit: it is marked `finished` and kept so
    // the coordinator window can still review and commit this session's work after it is closed.
    coop::clear();
    crate::agent::process::kill_all(); // reap any background dev servers/watchers we started
    println!("{}", style("bye.").dim());
    Ok(())
}

/// The plain line-REPL fallback (no sticky footer): used when stdout isn't a TTY or `AIZEN_NO_STICKY`
/// is set. You just type — a plain message is answered (chat), a task that needs tools uses them.
async fn run_menu_plain() -> Result<()> {
    splash::print();
    println!(
        "{}",
        style("Type to talk to the agent — it chats AND uses tools in one loop. /help for commands · Esc, Ctrl-C or /quit to exit.").dim()
    );
    {
        let (main, notes) = identity_banner();
        println!("{}", style(main).dim());
        for n in notes {
            println!("{}", style(n).color256(theme::WARN));
        }
        startup_update_probe();
    }

    let http = http_client()?;
    let mut model_label = cli_config::load()
        .model
        .unwrap_or_else(|| "(no model)".to_string());
    cli_config::pin_session_model(&model_label); // see the sticky REPL: per-window model, not per-disk
    let mut history: Vec<Message> = Vec::new();
    let mut input_history: Vec<String> = Vec::new(); // recallable past prompts (↑/↓ in the box)
    rebuild_system(&mut history, &model_label);
    install_exit_flush_handler(); // flush the live chat if the terminal window is closed (Windows ✕)
    if let Some(hint) = resume_hint() {
        tui::emit_line(&style(hint).dim().to_string());
    }

    loop {
        icons::set_tier(cli_config::load().icons.as_deref()); // refresh after a possible /config change
        print_status_line(&history, &model_label);
        let (line, mut images) = match read_input_box(&input_history)? {
            Some(l) => l,
            None => break,
        };
        let mut line = line.trim().to_string();
        // Drag-drop / typed / pasted image-file paths on the line → vision attachments (the other
        // half of Ctrl-O clipboard attach). Only real image files are pulled; prose is preserved.
        lift_image_attachments(&mut line, &mut images);
        if line.is_empty() && images.is_empty() {
            continue;
        }
        // Record for ↑/↓ recall (skip consecutive duplicates; text only — images aren't recallable).
        if !line.is_empty() && input_history.last().map(|p| p != &line).unwrap_or(true) {
            input_history.push(line.clone());
        }
        // What the user actually TYPED, captured before `@file` / `` !`cmd` `` expansion so effort
        // routing reads the request instead of its payload (mirror of the sticky path's `echo_src`).
        // Stays EMPTY for a slash command that expands into a prompt: there the expansion IS the
        // request, so classifying the `/name` the user typed would be the wrong text.
        let mut typed_src = String::new();
        // Slash command, or a typed input-box affordance (`#remember` / `!shell` / `@file`·`` !`cmd` ``).
        // Both are skipped when an image is attached — that's a vision message, sent verbatim.
        if images.is_empty() {
            // Bare `/` (+ Enter) → arrow-key command picker. Checked before `classify`, which reads
            // a lone slash as ordinary text (a message may legitimately begin with one).
            if line.trim() == "/" {
                match slash_menu(&mut history, &mut model_label).await {
                    SlashOutcome::Quit => break,
                    SlashOutcome::Submit(prompt) => line = prompt,
                    SlashOutcome::Continue => continue,
                }
            } else {
                // A leading `/` alone doesn't make a line a command — see `slash::classify`. Shared
                // with the retained input box and the host bot so the three cannot drift.
                match features::slash::classify(&line) {
                    features::slash::Verdict::Command { name, arg } => {
                        let rest = if arg.is_empty() {
                            name
                        } else {
                            format!("{name} {arg}")
                        };
                        match handle_slash(&rest, &mut history, &mut model_label).await {
                            SlashOutcome::Quit => break,
                            // A custom command expanded to a prompt → run it as a chat turn (not
                            // re-preprocessed).
                            SlashOutcome::Submit(prompt) => line = prompt,
                            SlashOutcome::Continue => continue,
                        }
                    }
                    // Near-miss: suggest and stop. Auto-running the closest match would let a
                    // slipped keystroke (`/claer`) wipe the conversation.
                    features::slash::Verdict::DidYouMean { typed, best } => {
                        tui::emit_line(
                            &style(format!("/{typed} — did you mean /{best}?"))
                                .dim()
                                .to_string(),
                        );
                        continue;
                    }
                    features::slash::Verdict::Chat => {
                        typed_src = line.clone();
                        match preprocess_input(&line) {
                            InputPre::Handled => continue, // #remember / !shell-escape — no turn
                            InputPre::Send(expanded) => line = expanded,
                        }
                    }
                }
            }
        }

        // A normal message → the unified chat+agent loop over the running conversation.
        let (base_url, api_key, model) = match resolve_endpoint(None, None, None) {
            Ok(t) => t,
            Err(_) => {
                println!(
                    "{}",
                    style("Not set up yet — run /config (or /model to pick a model).").dim()
                );
                continue;
            }
        };
        model_label = model.clone();
        // One resolved endpoint for the whole turn — the same value the retained REPL builds,
        // now that both hand the identical triple to the shared turn helpers.
        let ep = cli_config::ResolvedEndpoint {
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
        };
        agent::query_lang::set_expansion_endpoint(&ep);
        migrate_legacy_prompt_lanes(&mut history, &model);
        // Per-turn reasoning-effort auto-detect (mirrors the sticky REPL): classify what the user
        // TYPED, not the expanded payload — see the sticky path for why. Falls back to the finalized
        // text when there is no typed source (vision message, or a slash command that expanded).
        let effort_src = if typed_src.trim().is_empty() {
            &line
        } else {
            &typed_src
        };
        let eff = resolve_turn_effort(effort_src);
        cli_config::set_effort_override(eff.clone());
        println!("{}", effort_turn_line(eff.as_deref()));
        // Fold memory recall + codebase-index retrieval into the SENT content (not the cached
        // system lane) — see `fold_context_into_query`. `line` stays the original for persisted
        // history / display.
        seat_user_message(&line, images, &mut history, &model);
        // Snapshot the active persona so we can detect an in-turn switch (the `persona_create` tool)
        // and resync the system prompt at the turn boundary — prefix-cache safe, takes effect next msg.
        let persona_before = cli_config::load().persona;
        arm_lsp_session();
        let registry = match build_turn_registry(&http, &ep) {
            Ok(r) => r,
            Err(e) => {
                tui::note_line(&format!("{} {e}", style("error:").red()));
                history.pop();
                continue;
            }
        };
        // Unified ask/smart/yolo approval, with AIZEN_YES forcing yolo.
        let turn_cancel = crate::core::cancel::TurnCancel::new();
        let cfg = turn_agent_config(turn_cancel, &model, false);
        match run_agent_turn(&http, &ep, &cfg, &registry, &mut history).await {
            // `clarify` paused the turn — show the question, loop back for the answer (the next
            // typed message continues this conversation). No post-turn learning: not done yet.
            Ok(AgentOutcome {
                stop: StopReason::AwaitingInput(q),
                ..
            }) => {
                show_clarify(&q);
                autosave_last(&history, Some(&model)); // mirror of the sticky path: a paused turn is still a transcript
            }
            Ok(outcome) => {
                finish_turn(&outcome, persona_before, &mut history, &http, &ep).await;
            }
            Err(e) => {
                tui::note_line(&format!("{} {e}", style("error:").red()));
                if history.last().map(|m| m.role == "user").unwrap_or(false) {
                    history.pop(); // drop the failed user turn so history stays consistent
                }
            }
        }
        // Disarm the per-turn effort override so it never leaks into the next turn (mirror of the
        // sticky REPL's reset). Covers every branch above, incl. clarify/error.
        cli_config::clear_effort_override();
    }
    // Same graceful-exit flush as the sticky REPL: capture whatever's live even if the last turn
    // never reached the per-turn autosave.
    flush_live_session_on_exit();
    crate::agent::process::kill_all(); // reap any background dev servers/watchers we started
    println!("{}", style("bye.").dim());
    Ok(())
}

/// After a completed turn: if auto-compact is enabled and context usage crossed the threshold,
/// summarize older turns in place. Best-effort — a failed summary leaves the conversation intact.
async fn maybe_auto_compact(
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    let threshold = compact_threshold_pct();
    if threshold == 0 {
        return; // disabled
    }
    let (window, _) = resolve_ctx_window(model);
    let pct = session_tokens(history) as f64 / window as f64 * 100.0;
    if pct < threshold as f64 {
        return;
    }
    // The prefix cache is about to be invalidated anyway, so this is the one free moment to drop
    // the stale recall blocks accumulated on older user turns (see `strip_recall_blocks`).
    strip_recall_blocks(history);
    // tui::emit_line routes through the sticky footer when active, else prints a plain line.
    tui::emit_line(
        &style(format!(
            "↯ context {pct:.0}% ≥ {threshold}% — auto-compacting…"
        ))
        .dim()
        .to_string(),
    );
    match compact_history(history, http, base, key, model).await {
        Ok((b, a)) => tui::emit_line(
            &style(format!(
                "↯ auto-compacted: ~{} → ~{} tok",
                fmt_k(b),
                fmt_k(a)
            ))
            .color256(splash::ACCENT)
            .to_string(),
        ),
        Err(e) => tui::emit_line(&format!("{} {e}", style("auto-compact skipped:").dim())),
    }
}

/// Wrap a background / chore model call in the SAME per-call wall-clock deadline a sub-agent gets
/// ([`crate::agent::task_tool::subagent_call_timeout`], default 300s, `AIZEN_SUBAGENT_CALL_SECS`).
///
/// Every one of these is a NON-streaming `chat_with_tools` call — compaction / handoff summaries, the
/// end-of-turn secretary, persona reflection, memory reconcile, the oracle reviewer, persona distill.
/// None is streamed, so the streaming path's inter-event stall watchdog never applies; the shared
/// client carries no total-request ceiling (removed so a long *streamed* turn isn't cut — see
/// `http_client`); and `read_timeout` resets on every byte, so a gateway that keepalive-drips without
/// ever finishing the body parks the background task (or, for the by-hand ones, the CLI) forever. A
/// flat per-call cap is exactly right here — one call, one answer, no legitimate multi-minute stream.
/// On timeout it returns an ordinary `Err`, which every caller already treats as best-effort.
async fn chore_chat(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
) -> Result<client::ChatTurn> {
    let deadline = crate::agent::task_tool::subagent_call_timeout();
    match tokio::time::timeout(
        deadline,
        client::chat_with_tools(http, base, key, model, msgs, tools),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "chore model call exceeded {}s with no response (raise AIZEN_SUBAGENT_CALL_SECS)",
            deadline.as_secs()
        )),
    }
}

/// Is the `summarizer` role pointed at its OWN endpoint, or does it fall through to the main model?
///
/// Decides the secretary's input ceiling. When it falls through, every chore call bills the model
/// the user is actually coding with — on a large-context model that is the difference between a
/// chore and a real cost — so the transcript is capped much harder.
fn summarizer_is_dedicated(base: &str, key: &str, model: &str) -> bool {
    let ep = summarizer_endpoint(base, key, model);
    ep.model != model || ep.base_url != base
}

/// The end-of-turn secretary: ONE gated model call that files what the turn was worth.
///
/// Replaces `maybe_learn_memory` (regex extraction) + `maybe_learn_skill` (a second call) and folds
/// the persona episode in. Those two ran in OPPOSITE ORDERS in the retained and plain REPL loops,
/// so which of them saw the turn first depended on which loop you were in; one call cannot disagree
/// with itself.
///
/// Best-effort throughout: any failure means this turn taught nothing, never that the turn broke.
async fn maybe_run_secretary(
    history: &[Message],
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    use crate::memory::learning::secretary;

    if !memory_auto_learn_enabled() {
        return;
    }
    let start = match history.iter().rposition(|m| m.role == "user") {
        Some(i) => i,
        None => return,
    };
    let turn = &history[start..];

    // The user's ACTUAL words: history holds the folded message, so the recall block we injected
    // this turn has to come off first. Feeding it back would let the secretary re-emit a fact it was
    // just shown, and local reconciliation would read that as agreement.
    let user_text = turn
        .first()
        .and_then(|m| m.content.as_deref())
        .map(memory::strip_recall_prefix)
        .unwrap_or("")
        .trim()
        .to_string();
    if user_text.is_empty() {
        return;
    }
    // A turn that authored a CHARACTER was describing a fiction, not the user. Mining it leaks a
    // `persona-…` "fact" into user memory (it did, once — it polluted the verbosity profile).
    if memory::learning::turn_authored_persona(history) {
        return;
    }

    let tool_calls: usize = turn
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.tool_calls.len())
        .sum();
    let reason = secretary::gate(&user_text, tool_calls, turn_recovered_from_dead_end(turn));
    if !reason.fires() {
        return; // the common case: no model call at all
    }

    // Show the secretary the handles it may cite, with the text each one stood for.
    let injected: Vec<(String, String)> = {
        let live = memory::pending::current();
        if live.is_empty() {
            Vec::new()
        } else {
            let all = memory::store::load_all().unwrap_or_default();
            live.iter()
                .filter_map(|p| {
                    all.iter()
                        .find(|e| e.id == p.id)
                        .map(|e| (p.handle.clone(), e.body.clone()))
                })
                .collect()
        }
    };
    let injected_ids: Vec<String> = memory::pending::current()
        .into_iter()
        .map(|p| p.id)
        .collect();

    // A signal-only turn gets the SHORT transcript regardless of configuration: the durable content
    // is in what the user said, and a tool log would crowd it out of the budget.
    let cap =
        if reason == secretary::GateReason::Signal || !summarizer_is_dedicated(base, key, model) {
            secretary::CAP_TOKENS_SHARED_MODEL
        } else {
            secretary::CAP_TOKENS_OWN_ROLE
        };
    let input = secretary::build_input(&user_text, &render_transcript(turn), &injected, cap);

    let ep = summarizer_endpoint(base, key, model);
    let msgs = [
        Message::system(secretary::system_prompt()),
        Message::user(input),
    ];
    // Counted before the call, not after: a call that errors was still billed, and the point of the
    // number is cost per turn. Counting only successes would understate exactly the spend the gate
    // exists to control.
    memory::stats::note_secretary_call();
    let resp = match chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]).await {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    // `parse` never errors: garbage in yields an empty output, so a confused model costs one call.
    let out = secretary::parse(&resp.content.unwrap_or_default());

    // §8 metric 2 (injected-vs-used) is recorded HERE, before the empty-output early return: a gated
    // turn that was shown five facts and reported none of them useful is the single most informative
    // sample the ratio has. Dropping it would leave only the turns where recall worked, and the
    // metric would read high for exactly the store that needs fixing.
    //
    // Both halves come from one place so they cannot drift: the denominator is what the ledger
    // injected this turn, the numerator is the subset of THOSE handles the model cited (invented
    // handles resolve to nothing). Only gated turns are counted — an ungated turn was never asked.
    if !injected_ids.is_empty() {
        let used = memory::pending::resolve_used(&out.used).len() as u64;
        let shown = injected_ids.len() as u64;
        memory::stats::note_recall(shown, used);
        memory::learning::audit::recall(repl_session_id(), shown, used);
    }

    if out.is_empty() {
        return;
    }

    let report = secretary::apply_facts(&out, &injected_ids, repl_session_id());
    let confirmed_by_use = secretary::apply_used(&out);

    // Persona episode — CHARACTER only, and only when a character is actually active.
    if let Some(ep_prop) = out.episode.as_ref() {
        if persona_evolve_enabled() {
            if let Some(p) = persona::active() {
                let slug = skill::sanitize_name(&p.name);
                let _ = persona::self_mem::record_episode(&slug, &ep_prop.text, ep_prop.importance);
            }
        }
    }

    // Skill — save fresh, or fold into the existing one when the model asked to refine.
    // DEDUP (fix C, 2026-08-06): before auto-creating a NEW skill, compare the proposed `when` +
    // `steps` against every existing skill with `match_similarity`. Three identical "verify GitHub
    // Actions YAML" skills were observed on the same day because the only collision key was the slug,
    // and minor wording changes in the name produced different slugs. Now a new skill whose trigger
    // resembles an existing one gets routed to `refine` instead of spawning a duplicate.
    if let Some(sk) = out.skill.as_ref() {
        if auto_skill_learn_enabled() {
            use crate::memory::learning::match_text;
            let slug = skill::sanitize_name(&sk.name);
            let all_skills = skill::list();
            let exact_exists = all_skills
                .iter()
                .any(|s| skill::sanitize_name(&s.name) == slug);
            // Check if any existing skill has a semantically similar trigger.
            let similar_skill = if !exact_exists {
                all_skills.iter().find(|s| {
                    let trigger_sim = match_text::match_similarity(&sk.when, &s.when);
                    let body_sim = match_text::match_similarity(&sk.steps, &s.body);
                    // Both trigger AND body must resemble — trigger alone would merge skills that
                    // fire in the same situation but do different things.
                    trigger_sim >= 0.45 && body_sim >= 0.35
                })
            } else {
                None
            };
            let done = if exact_exists {
                // Only fold when the model MEANT to; otherwise a same-named skill is a collision to
                // leave alone, not a licence to overwrite the user's procedure.
                sk.refine && skill::refine(&sk.name, &sk.steps, None, Some(&sk.when)).is_ok()
            } else if let Some(existing) = similar_skill {
                // Similar enough to refine rather than duplicate. Route to the existing skill.
                skill::refine(&existing.name, &sk.steps, None, Some(&sk.when)).is_ok()
            } else {
                skill::save_scoped(&sk.name, "", &sk.when, &sk.steps, true).is_ok()
            };
            if done {
                let label = if exact_exists || similar_skill.is_some() {
                    "refined"
                } else {
                    "learned"
                };
                let display_name = similar_skill.map(|s| s.name.as_str()).unwrap_or(&sk.name);
                tui::emit_line(
                    &style(format!(
                        "{}{label} skill '{display_name}' — /skills to view",
                        icons::g(icons::learned()),
                    ))
                    .color256(splash::ACCENT)
                    .to_string(),
                );
            }
        }
    }

    let n_new = report.added.len();
    let n_conf = report.confirmed.len() + confirmed_by_use;
    let n_queue = report.queued_review.len();
    if n_new > 0 || n_conf > 0 || n_queue > 0 {
        let mut parts: Vec<String> = Vec::new();
        if n_new > 0 {
            parts.push(format!("remembered {n_new}"));
        }
        if n_conf > 0 {
            parts.push(format!("confirmed {n_conf}"));
        }
        if n_queue > 0 {
            parts.push(format!("{n_queue} to review"));
        }
        tui::emit_line(
            &style(format!(
                "{}{} — /memory to view",
                icons::g(icons::learned()),
                parts.join(", ")
            ))
            .color256(splash::ACCENT)
            .dim()
            .to_string(),
        );
    }
}

/// Did this turn RECOVER from a dead end — a tool result errored, then a LATER tool result in the
/// same turn succeeded? That recovery is a hard-won procedure worth distilling even on a short turn.
/// Tool errors are fed back as result strings starting with `error:` (the loop's convention).
fn turn_recovered_from_dead_end(turn: &[Message]) -> bool {
    let mut saw_error = false;
    for m in turn.iter().filter(|m| m.role == "tool") {
        let is_err = m
            .content
            .as_deref()
            .unwrap_or("")
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("error:");
        if is_err {
            saw_error = true;
        } else if saw_error {
            return true; // a success after an earlier error → the agent worked through a dead end
        }
    }
    false
}

/// One stable session id for the whole REPL process, so per-turn auto-learn reinforces facts
/// across turns of ONE session (not a fresh "session" each turn, which would over-count
/// `session_count` and wrongly accelerate review/promotion).
fn repl_session_id() -> &'static str {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(crate::memory::learning::default_session_id)
}

fn memory_auto_learn_enabled() -> bool {
    cli_config::load().memory_auto_learn.unwrap_or(true)
}

/// Decide THIS turn's reasoning effort. When auto-detect is ON (default), classify the user's
/// fully-expanded message (keyword ladder → complexity heuristic); a hit forces that tier, else we
/// fall back to the configured `reasoning_effort` (which may itself be `None` ⇒ omit the field).
/// PURE-ish wrapper (loads config) around the pure `core::effort::classify_effort`. The result is
/// armed into the per-turn override cell read by the LLM client, and cleared at turn end.
fn resolve_turn_effort(line: &str) -> Option<String> {
    // Ultimate mode pins max effort every turn (auto-detect is bypassed) — the aizen `ultracode`.
    if cli_config::ultimate_enabled() {
        return Some("max".to_string());
    }
    if cli_config::auto_effort_enabled() {
        // P3: adaptive routing lets the complexity heuristic climb to `xhigh` on the hardest turns.
        let adaptive = cli_config::adaptive_effort_enabled();
        if let Some(e) = crate::core::effort::classify_effort_with(line, adaptive) {
            return Some(e.as_str().to_string());
        }
    }
    cli_config::load().reasoning_effort.clone()
}

/// The per-turn "effort: <tier>" status line, tinted to match the slider's tier colours (auto =
/// moonlight · low = green · medium = dim silver · high = gold) so the whole effort feature reads as
/// one system. `None` ⇒ the field is omitted this turn → shown as a faint "default".
fn effort_turn_line(eff: Option<&str>) -> String {
    // low = green, medium = dim silver; the three "hot" rungs escalate high → xhigh → max
    // (gold → bold gold → salmon) so the eye can tell them apart at a glance.
    let styled = match eff {
        Some("low") => console::style("low".to_string()).color256(theme::OK),
        Some("medium") => console::style("medium".to_string()).color256(theme::ACCENT_DIM),
        Some("high") => console::style("high".to_string()).color256(theme::WARN),
        Some("xhigh") => console::style("xhigh".to_string())
            .color256(theme::WARN)
            .bold(),
        Some("max") => console::style("max".to_string())
            .color256(theme::ERR)
            .bold(),
        Some(other) => console::style(other.to_string()).color256(theme::ACCENT),
        None => console::style("default".to_string()).color256(theme::FAINT),
    };
    format!("{} {}", theme::faint("  effort:"), styled)
}

/// The current effort setting as a slider index: 0 = auto (auto-detect ON, no pinned tier), else the
/// pinned tier (1=low · 2=medium · 3=high). A pinned-but-unknown effort string, or auto-off with no
/// pin, both fall back to `auto` so the slider always opens on a valid stop.
fn effort_slider_start() -> usize {
    let cfg = cli_config::load();
    if cli_config::auto_effort_enabled() {
        return 0; // auto ON ⇒ the "auto" stop, regardless of any stale pinned value
    }
    match cfg.reasoning_effort.as_deref() {
        Some("low") => 1,
        Some("medium") => 2,
        Some("high") => 3,
        Some("xhigh") => 4,
        Some("max") => 5,
        _ => 0,
    }
}

/// Apply a slider choice to the config and persist it. `0` ⇒ auto (auto_effort=None, clear the pin);
/// `1..=5` ⇒ pin low/medium/high/xhigh/max and turn auto off — the exact same writes as `/effort auto`
/// and `/effort low|medium|high|xhigh|max`, so the slider and the text commands stay in lockstep.
fn apply_effort_choice(idx: usize) {
    let mut cfg = cli_config::load();
    let msg = match idx {
        1..=5 => {
            let tier = ["", "low", "medium", "high", "xhigh", "max"][idx];
            cfg.reasoning_effort = Some(tier.to_string());
            cfg.auto_effort = Some(false);
            format!("effort pinned to {tier} (auto off) — every turn now sends reasoning_effort={tier}.")
        }
        _ => {
            cfg.auto_effort = None; // None ⇒ auto ON (the default)
            cfg.reasoning_effort = None; // clear any stale pin so auto isn't shadowed
            "effort auto ON — each turn's effort is detected from your message (keyword + complexity).".to_string()
        }
    };
    match cli_config::save(&cfg) {
        Ok(_) => tui::emit_line(&style(msg).color256(splash::ACCENT).to_string()),
        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
    }
}

/// The plain text status report for `/effort status` (and the off-TTY fallback of the bare `/effort`).
fn effort_status_report() {
    let cfg = cli_config::load();
    let auto = if cli_config::auto_effort_enabled() {
        "on"
    } else {
        "off"
    };
    let fixed = cfg
        .reasoning_effort
        .as_deref()
        .unwrap_or("(none — omitted)");
    tui::emit_line(
        &style(format!(
            "effort: auto-detect {auto} · fixed reasoning_effort {fixed}\n\
             /effort auto|off · /effort low|medium|high (pins it, turns auto off) · /effort none (clear)"
        ))
        .dim()
        .to_string(),
    );
    if std::env::var("AIZEN_AUTO_EFFORT").is_ok() {
        tui::emit_line(
            &style("(note: AIZEN_AUTO_EFFORT is set — it overrides the auto toggle)")
                .dim()
                .to_string(),
        );
    }
}

/// Bare `/effort` → the animated drag slider. Opens on the current setting; a commit persists the
/// choice, Esc keeps things as-is. Off-TTY the slider returns `None` immediately, so we fall back to
/// the text report instead of leaving the user with no output.
fn effort_slider_flow() {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        effort_status_report();
        return;
    }
    match tui::effort_slider(effort_slider_start()) {
        Some(idx) => apply_effort_choice(idx),
        None => tui::emit_line(&style("(effort unchanged)").dim().to_string()),
    }
}

/// After a completed turn: passively learn durable user/project facts from the user's last message.
/// FREE — regex extraction, no model call — through the SAME pipeline as `aizen memory learn`
/// (sanitize-to-fact → write-time threat-scan → confidence-route → consolidate → store, with
/// anti-bloat). Core promotion stays human-gated (`auto_confirm_core = Some(false)`): a would-be
/// core fact is downgraded to a normal store entry and NEVER silently mutates the always-on frozen
/// prefix (prefix-cache byte-stability is sacred). Best-effort + visible; never disrupts the REPL.
fn maybe_learn_memory(history: &[Message]) {
    use crate::memory::learning::{self, LearnOptions};
    if !memory_auto_learn_enabled() {
        return;
    }
    let user_text = match history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
    {
        Some(t) => t,
        None => return,
    };
    if user_text.trim().is_empty() {
        return;
    }
    // If THIS turn authored a character (the `persona_create` tool fired), the user's message was
    // describing a FICTIONAL persona, not stating their own preferences — mining it would leak a
    // `persona-…` "fact" into user memory. Skip learning for the whole turn. (The regex intent-gate
    // inside `ingest` is the first, heuristic line of defense; this fact-based gate catches phrasings
    // it misses. Lives as a unit-tested helper so this loop can't silently drop it in a refactor.)
    if learning::turn_authored_persona(history) {
        return;
    }
    let opts = LearnOptions {
        session_id: repl_session_id().to_string(),
        auto_confirm_core: Some(false), // never auto-mutate the frozen core; downgrade to store
        dry_run: false,
    };
    let report = match learning::ingest(&user_text, &opts) {
        Ok(r) => r,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let n_durable = report.added.len() + report.reinforced.len();
    let n_session = report.session_notes.len();
    if n_durable > 0 {
        tui::emit_line(
            &style(format!(
                "{}remembered {n_durable} fact{} — /memory to view",
                icons::g(icons::learned()),
                if n_durable == 1 { "" } else { "s" }
            ))
            .color256(splash::ACCENT)
            .dim()
            .to_string(),
        );
    } else if n_session > 0 {
        // Inferred → session working memory only (not durable). Quiet, dim.
        tui::emit_line(
            &style(format!(
                "{}noted {n_session} for this session (not saved permanently)",
                icons::g(icons::learned()),
            ))
            .dim()
            .to_string(),
        );
    }
}

/// After a completed turn: if a persona is active, distill its accumulated episodes into durable
/// character insights when enough formative weight has piled up.
///
/// This used to also RECORD the turn's episode from a regex gate. That half moved into
/// [`maybe_run_secretary`], which already reads the finished turn — two writers meant one formative
/// moment landed twice, once as the gate's templated body and once in the model's own words. What
/// remains is the periodic tier: reflection is about the accumulation, not about this turn, so it
/// needs no `history` at all.
///
/// Best-effort + visible — never disrupts the REPL.
async fn maybe_evolve_persona(http: &reqwest::Client, base: &str, key: &str, model: &str) {
    if !persona_evolve_enabled() {
        return;
    }
    let persona = match persona::active() {
        Some(p) => p,
        None => return, // no character active → nothing to evolve
    };
    let slug = skill::sanitize_name(&persona.name);
    if persona::self_mem::should_reflect(&slug) {
        run_persona_reflection(&persona, &slug, http, base, key, model).await;
    }
}

/// The reflection call: synthesize recent episodes into 1-3 durable insights for this character.
async fn run_persona_reflection(
    persona: &persona::Persona,
    slug: &str,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    let episodes = persona::self_mem::recent_episode_bodies(slug, 20);
    if episodes.len() < persona::self_mem::REFLECT_MIN_EPISODES {
        return;
    }
    let (sys, usr) =
        persona::reflect::build_reflection_prompt(&persona.name, &persona.role, &episodes);
    // Chore-class synthesis call → billed to the summarizer role, like every other harness chore.
    let ep = summarizer_endpoint(base, key, model);
    let resp = match chore_chat(
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
        &[Message::system(sys), Message::user(usr)],
        &[],
    )
    .await
    {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let content = resp.content.unwrap_or_default();
    let json = match extract_json_object(&content) {
        Some(j) => j,
        None => return,
    };
    let insights = persona::reflect::parse_insights(json);
    if insights.is_empty() {
        return;
    }
    let mut saved = 0usize;
    for ins in &insights {
        if let Ok(id) = persona::self_mem::save_insight(slug, &ins.text, ins.importance) {
            saved += 1;
            // Cross-kind Hebbian edge: an insight distilled while these facts were in play is
            // associated with them. Best-effort — a graph write never affects the reflection.
            persona::self_mem::note_insight_cofire(slug, &id);
        }
    }
    if saved > 0 {
        tui::emit_line(
            &style(format!(
                "{}{} reflected — +{saved} insight(s) from recent sessions (/persona to view)",
                icons::g(icons::learned()),
                persona.name
            ))
            .color256(splash::ACCENT)
            .to_string(),
        );
    }
}

/// Build prompt lanes around a caller-selected frozen core. Keeping the lifecycle choice OUT of this
/// helper makes every call site say whether it is opening a fresh conversation (refresh/adopt) or
/// merely rewriting lanes inside the current one (read the already-adopted bytes).
fn system_prompt_bundle_with_core(model: &str, frozen: &str) -> agent::PromptBundle {
    system_prompt_bundle_in(model, frozen, None)
}

/// As `system_prompt_bundle_with_core`, but stating an EXPLICIT working directory in the prompt.
/// The hostbot daemon passes its lane's cwd: the prompt tells the model where it is working, and
/// under several concurrent bots the process cwd is not that answer for any of them.
fn system_prompt_bundle_in(
    model: &str,
    frozen: &str,
    root: Option<&std::path::Path>,
) -> agent::PromptBundle {
    let cwd = root
        .map(|p| p.display().to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut bundle = agent::build_top_level_system_prompt_bundle(
        &cwd,
        std::env::consts::OS,
        &date,
        model,
        Some(frozen),
    );
    // L2 session working memory (temporary, budget-capped). Empty → no tag (zero cost).
    let sess_budget = memory::settings().session_mem_max_tokens;
    if let Some(block) = memory::session_mem::process_prompt_block(sess_budget) {
        bundle.dynamic.push('\n');
        bundle.dynamic.push_str(&block);
        bundle.dynamic.push('\n');
    }
    bundle
}

/// A TRUE conversation boundary: promote pending memory, rebuild from the current store, and adopt
/// the result before constructing a fresh prompt prefix. Startup, `/clear`, `/handoff`, session load,
/// and one-shot/captured runs are the only callers that should use this path.
fn refreshed_system_prompt_bundle(model: &str) -> agent::PromptBundle {
    let frozen = memory::refresh_frozen_core();
    system_prompt_bundle_with_core(model, &frozen)
}

/// Same-conversation lane rewrite: reuse the already-adopted core byte-for-byte. A retrieval,
/// reinforcement, or memory write during this conversation may stage `core.next`, but must not
/// mutate the cached prefix or promote it before the next conversation boundary.
fn active_system_prompt_bundle(model: &str) -> agent::PromptBundle {
    let frozen = memory::active_frozen_core();
    system_prompt_bundle_with_core(model, &frozen)
}

/// The two bundle builders above, parameterized by the hostbot lane's working directory and persona.
/// Both are needed together because the persona override must be armed for the (synchronous) prompt
/// assembly and released immediately after — see `persona::with_override`.
fn hostbot_prompt_bundle(
    model: &str,
    root: &std::path::Path,
    persona: Option<String>,
    boundary: bool,
) -> agent::PromptBundle {
    persona::with_override(persona, || {
        let frozen = if boundary {
            memory::refresh_frozen_core()
        } else {
            memory::active_frozen_core()
        };
        system_prompt_bundle_in(model, &frozen, Some(root))
    })
}

/// Seed both system lanes for a brand-new conversation.
fn seed_prompt_lanes(history: &mut Vec<Message>, model: &str) {
    history.clear();
    let bundle = refreshed_system_prompt_bundle(model);
    history.push(Message::system(bundle.stable));
    if !bundle.dynamic.trim().is_empty() {
        history.push(Message::system(bundle.dynamic));
    }
}

/// Replace a persisted zero/one-system legacy prefix with the current two-lane prompt.
/// Histories already carrying both lanes are left byte-identical.
fn migrate_legacy_prompt_lanes(history: &mut Vec<Message>, model: &str) {
    let lead = agent::compact::leading_system_count(history);
    if lead >= 2 {
        return;
    }
    let tail = history.get(lead..).unwrap_or_default().to_vec();
    seed_prompt_lanes(history, model);
    history.extend(tail);
}

/// Per-turn budget (tokens, chars/4 estimate) for the `/init` codebase-retrieval block folded into
/// the CURRENT user turn (see [`fold_retrieval_into_query`]). Small enough to stay well under the
/// frozen-core/session budgets but big enough for ~5-8 chunks with attribution.
const CODEBASE_RETRIEVAL_BUDGET_TOKENS: usize = 1500;

/// Fresh user-turn boundary: refresh only the dynamic lane, preserving stable index 0 byte-for-byte.
fn refresh_dynamic_prompt_lane(history: &mut Vec<Message>, model: &str) {
    migrate_legacy_prompt_lanes(history, model);
    let dynamic = active_system_prompt_bundle(model).dynamic;
    let lead = agent::compact::leading_system_count(history);
    if dynamic.trim().is_empty() {
        if lead > 1 {
            history.remove(1);
        }
    } else if lead > 1 {
        history[1] = Message::system(dynamic);
    } else {
        history.insert(1, Message::system(dynamic));
    }
}

/// Rewrite BOTH system lanes in place, preserving every non-system message.
///
/// For settings changes that alter the STABLE lane — `/model` and `/config` both do, since the model
/// name, prompt tier and `<project_context>` live at index 0 — but must NOT end the conversation.
/// `rebuild_system` cannot serve here: it calls `seed_prompt_lanes`, which starts with
/// `history.clear()`, so using it for a settings change silently threw away the whole chat (the user
/// went to `/config` to retune the context and came back to an empty thread).
///
/// Session working memory is deliberately KEPT: this is the same conversation, so its scratch notes
/// are still valid. That is the other half of why `/config` must not route through `rebuild_system`,
/// which drops them as part of starting a new thread.
/// Splice a caller-selected prompt bundle over the leading prompt lanes while preserving every
/// conversation message (including a handoff seed, which is a third system message but NOT part of
/// the two-lane prefix).
fn splice_prompt_lanes(history: &mut Vec<Message>, bundle: agent::PromptBundle) {
    let lead = agent::compact::leading_system_count(history);
    let mut lanes = vec![Message::system(bundle.stable)];
    if !bundle.dynamic.trim().is_empty() {
        lanes.push(Message::system(bundle.dynamic));
    }
    history.splice(0..lead, lanes);
}

/// Same-conversation rewrite (`/config`, `/model`, persona change): keep the active core stable.
fn refresh_prompt_lanes_in_place(history: &mut Vec<Message>, model: &str) {
    splice_prompt_lanes(history, active_system_prompt_bundle(model));
}

/// Thread switch (`/resume`, session/time-machine restore): refresh/adopt memory for the new
/// conversation before rebuilding the current-project prompt lanes around its saved transcript.
pub(crate) fn refresh_prompt_lanes_for_thread_switch(history: &mut Vec<Message>, model: &str) {
    splice_prompt_lanes(history, refreshed_system_prompt_bundle(model));
}

/// Automatic codebase RAG, folded into the CURRENT user turn (NOT the dynamic system lane).
///
/// When `/init` has built an index, the top-ranked chunks (path + line range + real content,
/// source-attributed) are prepended to the user's message so the model sees relevant code before it
/// even calls a tool. Placing it on the user turn — the volatile, already-uncached message — keeps
/// index 1 (the dynamic system lane) byte-stable, so the provider's prefix cache still covers the
/// whole transcript tail up to the last stable turn. Folding into the dynamic lane instead would
/// vary index 1 every turn and force the entire transcript after it to re-bill uncached (the
/// Anthropic prefix-cache breakpoint sits on the last stable assistant/tool message).
///
/// Returns the message content to send. The caller keeps the ORIGINAL `query` for checkpoint /
/// display / persisted history — only the sent content carries the (ephemeral, per-turn) block.
/// No-op passthrough when there is no index / no query terms / nothing clears the relevance gate.
fn fold_retrieval_into_query(query: &str) -> String {
    if query.trim().is_empty() {
        return query.to_string();
    }
    // Kick a background drift check: if source files changed since the last /init, an incremental
    // rebuild runs off-turn so the NEXT turn sees fresh context. Never blocks this turn (#17).
    crate::agent::codebase::ensure_fresh();
    match crate::agent::codebase::retrieval_block(query, CODEBASE_RETRIEVAL_BUDGET_TOKENS) {
        Some(block) => format!("{block}\n\n{query}"),
        None => query.to_string(),
    }
}

/// Per-turn budget (tokens) for the memory recall block folded into the CURRENT user turn.
/// Deliberately an order of magnitude under the codebase budget: this carries a handful of
/// one-line facts, not source, and it is spent on every gated turn.
const MEMORY_RECALL_BUDGET_TOKENS: usize = 300;

/// Fold BOTH per-turn context blocks into the sent content: memory recall, then codebase RAG.
///
/// Same discipline as [`fold_retrieval_into_query`] and for the same reason — the blocks ride on the
/// **user turn**, which is already uncached, so system lanes 0/1 stay byte-stable and the provider's
/// prefix cache keeps covering the transcript tail (invariant I1).
///
/// Memory goes FIRST so the standing facts ("reply in Vietnamese", "windows-sys is pinned") are read
/// before the code they qualify. The recall block also seats its handle→id pairs in the pending
/// ledger, which is what lets a later `used` report confirm only facts that were actually shown.
///
/// `query` itself is never modified: the caller keeps it for checkpoint / display / persisted
/// history, so the durable transcript holds the user's real words, not our scaffolding.
fn fold_context_into_query(query: &str) -> String {
    // The turn counter lives here because this is the one point BOTH REPL loops pass through exactly
    // once per user message. Counting inside the agent loop would count iterations, and metric 1's
    // denominator ("live facts per turn") has to mean turns the user drove.
    memory::stats::note_turn();
    let mut out = fold_retrieval_into_query(query);
    // Skills that actually fit THIS question, gated on the same coverage threshold as recall. The
    // always-on `<skills>` index names every applicable procedure regardless of the request; this
    // block is what makes the fitting ones salient without spending the system lane's byte-stable
    // budget on the ones that don't. Folded ABOVE the code but BELOW the facts, matching the
    // "standing truth → how-to → source" reading order.
    if let Some(block) = skills::turn_block(query, skills::SKILL_TURN_BUDGET_TOKENS) {
        out = format!("{block}\n\n{out}");
    }
    if let Some((block, pairs)) = memory::recall_block(query, MEMORY_RECALL_BUDGET_TOKENS) {
        memory::pending::open_turn(pairs);
        out = format!("{block}\n\n{out}");
    }
    out
}

/// Drop per-turn context blocks (memory recall, gated skills) from user turns already in `history`.
///
/// Each block was true for the turn it rode in on. Left in place they accumulate — ten turns of
/// standing facts re-stated ten times — and, worse, an older block can contradict a newer one with
/// nothing in the transcript marking which came later, so the model has to guess.
///
/// Called only from [`maybe_auto_compact`], at the moment the prefix cache is being invalidated
/// anyway: rewriting a user turn at any other time would break cache coverage for the whole tail,
/// costing more than the tokens it saves.
///
/// Matches on [`memory::RECALL_MARKER`] / [`skills::SKILL_MARKER`] at the start of the content and
/// cuts through the first blank line. Anything the user actually typed survives, including a message
/// that merely mentions the phrase — a marker has to be at position 0, which only our own folding
/// produces. Both are peeled in a loop because a turn carries them stacked (recall, then skills):
/// removing the outer one promotes the inner one to position 0.
fn strip_recall_blocks(history: &mut [Message]) {
    for m in history.iter_mut() {
        if m.role != "user" {
            continue;
        }
        let Some(content) = m.content.as_deref() else {
            continue;
        };
        let mut cur = content;
        loop {
            let next = strip_skill_prefix(memory::strip_recall_prefix(cur));
            if next.len() == cur.len() {
                break;
            }
            cur = next;
        }
        if cur.len() != content.len() {
            m.content = Some(cur.to_string());
        }
    }
}

/// Peel one leading gated-skill block, mirroring [`memory::strip_recall_prefix`].
fn strip_skill_prefix(content: &str) -> &str {
    if !content.starts_with(skills::SKILL_MARKER) {
        return content;
    }
    match content.split_once("\n\n") {
        Some((_, rest)) => rest,
        None => content,
    }
}

/// Everything a THREAD SWITCH must reset besides history itself: session scratch memory, todos,
/// the cost tally, destructive-op session grants, and browser page @refs. `/clear`, `/handoff`,
/// `/resume`, `/sessions` restore and `/recover` all route here so a fresh or restored thread
/// never inherits the previous one's state (the classic leak: a restored conversation still
/// "allowed" the old thread's destructive ops and showed its cost).
fn reset_per_session_state() {
    memory::session_mem::clear_process_session_mem();
    // The new transcript never contained the old recall block, so its handles now point at facts
    // the model cannot see — and a stale `last_ids` would suppress the first block of the new
    // thread as a "duplicate" of one that is no longer in context.
    memory::pending::clear();
    crate::agent::todo::clear();
    client::cost_meter().reset();
    tui::reset_session_allow();
    #[cfg(feature = "browser")]
    crate::agent::browser::release_active();
}

/// Reset the conversation to just the system prompt (fresh session / model change). Rebuilds the
/// frozen core from the current memory store so newly added `type=user` facts / STYLE are injected.
/// Drops session working memory — a new thread does not inherit this session's scratch notes.
fn rebuild_system(history: &mut Vec<Message>, model: &str) {
    memory::session_mem::clear_process_session_mem();
    seed_prompt_lanes(history, model);
}

/// Replace the system lanes in place WITHOUT clearing the conversation — used when switching
/// persona mid-chat so the new character applies but the history is preserved.
fn update_system_prompt(history: &mut Vec<Message>, model: &str) {
    refresh_dynamic_prompt_lane(history, model);
}

/// Approximate context window (tokens) for a model, by name pattern. A rough heuristic for the
/// `% context` HUD only — not a hard cap (the upstream enforces the real limit). Defaults to 128K.
fn ctx_window_for(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("1m") {
        1_000_000 // explicit 1M-context variants (e.g. opus-4-8-1m-thinking) — checked before the family heuristics
    } else if m.contains("gemini") {
        1_000_000
    } else if m.contains("claude")
        || m.contains("opus")
        || m.contains("sonnet")
        || m.contains("haiku")
    {
        200_000
    } else if m.contains("gpt-4.1") || m.contains("o3") || m.contains("o4") {
        1_000_000
    } else if m.contains("deepseek") {
        64_000
    } else {
        128_000 // gpt-4o family + safe default
    }
}

/// A 10-cell context-fill bar, coloured by pressure using the semantic palette (P-ctx4): OK below
/// 50%, WARN gold from 50%, ERR salmon from 80% — the same green/gold/salmon meanings the rest of
/// the UI uses, instead of bespoke 256-colour indices.
fn ctx_bar(pct: f64) -> String {
    const CELLS: usize = 10;
    let filled = ((pct / 100.0) * CELLS as f64)
        .round()
        .clamp(0.0, CELLS as f64) as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled));
    let color: u8 = if pct >= 80.0 {
        theme::ERR
    } else if pct >= 50.0 {
        theme::WARN
    } else {
        theme::OK
    };
    style(bar).color256(color).to_string()
}

/// The effective window from an explicit/configured value (when present) over the name heuristic.
/// Returns `(tokens, was_configured)`. Pure — callers pass the value (lets the wizard compute it
/// against unsaved in-memory config).
pub(crate) fn effective_ctx_window(model: &str, configured: Option<usize>) -> (usize, bool) {
    match configured {
        Some(w) if w > 0 => (w, true),
        _ => (ctx_window_for(model), false),
    }
}

/// The effective context window for `model`: a provider-reported/manually-set value in config (when
/// it matches the active model) wins over the name heuristic. Returns `(tokens, was_configured)`.
fn resolve_ctx_window(model: &str) -> (usize, bool) {
    let cfg = cli_config::load();
    let configured = cfg
        .model_context_window
        .filter(|_| cfg.model.as_deref() == Some(model));
    effective_ctx_window(model, configured)
}

/// Rough session size in tokens — shared by the HUD + auto-compact. Delegates to the agent
/// estimator (content + tool-call payloads + envelopes) plus the tool-schema overhead the loop
/// last published, so the HUD and the mid-loop guards agree on request size.
fn session_tokens(history: &[Message]) -> usize {
    history
        .iter()
        .map(agent::estimate_message_tokens)
        .sum::<usize>()
        + agent::schema_overhead_tokens()
}

/// Compact a token count for display: `12.4K` / `300`.
fn fmt_k(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// `/cost` — session token accounting + (when rates are set) an estimated $ cost. Honest by design:
/// shows REAL provider-reported tokens when the endpoint sends `usage`, else the chars/4 context
/// estimate clearly labelled — and never invents a price or a credit balance.
fn print_cost(history: &[Message], model: &str) {
    let (p, c, calls) = client::cost_meter().snapshot();
    let cfg = cli_config::load();
    if calls > 0 {
        let total = p + c;
        let mut line = format!(
            "{}  {} in + {} out = {} tok  ({} call{} reported usage)",
            style("💰 session usage").color256(splash::ACCENT).bold(),
            fmt_k(p as usize),
            fmt_k(c as usize),
            fmt_k(total as usize),
            calls,
            if calls == 1 { "" } else { "s" },
        );
        match (cfg.price_in, cfg.price_out) {
            (Some(pin), Some(pout)) => {
                let cost = p as f64 / 1_000_000.0 * pin + c as f64 / 1_000_000.0 * pout;
                line.push_str(&format!(
                    "  ·  {}",
                    style(format!("est ${cost:.4} (@ ${pin}/${pout} per 1M in/out)")).color256(splash::ACCENT)
                ));
            }
            _ => line.push_str(&format!(
                "  ·  {}",
                style("set rates for a $ estimate: aizen config set --price-in <$/1M> --price-out <$/1M>").dim()
            )),
        }
        // Prompt-cache payoff (only when the provider reported cache reads → confirms caching works).
        let cached = client::cost_meter().cache_read();
        if cached > 0 {
            line.push_str(&format!(
                "  ·  {}",
                style(format!("{} cached @ ~0.1× in", fmt_k(cached as usize))).color256(theme::OK)
            ));
        }
        tui::emit_line(&line);
    } else {
        // No real usage from the provider → fall back to the context-size estimate (not a $ figure).
        let est = session_tokens(history);
        let (window, _) = resolve_ctx_window(model);
        tui::emit_line(&format!(
            "{}  ~{} tok in context · window {} {}",
            style("📊 estimated").color256(splash::ACCENT).bold(),
            fmt_k(est),
            fmt_k(window),
            style("(chars/4 — the provider didn't report token usage, so no per-call $ to show)")
                .dim()
        ));
    }
}

/// Decompose the live system prompt into its named blocks by XML tag, returning (label, char count)
/// for the leftover base instructions plus every block actually present. Pure (byte-index scan over
/// ASCII tags) so it's unit-testable; char counts ÷4 ≈ tokens, the same basis the HUD estimator uses.
fn system_block_chars(system: &str) -> Vec<(&'static str, usize)> {
    // (display label, tag) in build order — an absent block contributes nothing.
    const BLOCKS: &[(&str, &str)] = &[
        ("environment", "environment"),
        ("agent identity", "agent_identity"),
        ("persona", "persona"),
        ("persona memory", "self"),
        ("user memory", "user_memory"),
        ("skills index", "skills"),
        ("project context", "project_context"),
        ("agents index", "agents"),
    ];
    let mut rows = Vec::new();
    let mut tagged = 0usize;
    for (label, tag) in BLOCKS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let (Some(s), Some(e)) = (system.find(&open), system.find(&close)) {
            if e >= s {
                // Tags are ASCII, so byte slicing lands on char boundaries.
                let c = system[s..e + close.len()].chars().count();
                tagged += c;
                rows.push((*label, c));
            }
        }
    }
    let base = system.chars().count().saturating_sub(tagged);
    let mut out = vec![("base instructions", base)];
    out.extend(rows);
    out
}

/// Render the `/compact` result as a small tree: a headline with the token delta, then one `└` leaf
/// per file the collapsed turns had referenced and one for the skills they had loaded. The leaves are
/// what makes compaction feel non-lossy — the dense summary note is invisible, but this shows at a
/// glance the concrete context (which files, which skills) those turns carried, harvested by
/// [`agent::compact::context_touchpoints`] BEFORE the collapse.
fn print_compact_summary(before: usize, after: usize, tp: &agent::compact::Touchpoints) {
    let saved = before.saturating_sub(after);
    tui::emit_line(&format!(
        "{}  {} → {} tok{}",
        style("✳ Compacted").color256(splash::ACCENT).bold(),
        style(format!("~{}", fmt_k(before))).dim(),
        style(format!("~{}", fmt_k(after))).color256(splash::ACCENT),
        if saved > 0 {
            style(format!("  · freed ~{}", fmt_k(saved)))
                .dim()
                .to_string()
        } else {
            String::new()
        },
    ));
    let leaf = style("  └").color256(theme::FAINT).to_string();
    for f in &tp.files {
        tui::emit_line(&format!(
            "{leaf} {} {}",
            style("Referenced file").dim(),
            style(f).color256(theme::ACCENT_DIM)
        ));
    }
    if !tp.skills.is_empty() {
        tui::emit_line(&format!(
            "{leaf} {} ({})",
            style("Skills restored").dim(),
            style(tp.skills.join(", ")).color256(theme::ACCENT_DIM),
        ));
    }
    if tp.files.is_empty() && tp.skills.is_empty() {
        tui::emit_line(&format!(
            "{leaf} {}",
            style("no files or skills to carry forward").dim()
        ));
    }
}

/// `/context` — where the tokens are going right now: the system prompt split into its blocks, the
/// tool-schema overhead (rides every request, lives in no message), and the conversation split by
/// role. Estimated (chars/4) — the same honest basis the HUD + auto-compact use; `/cost` shows the
/// provider's REAL billed count when the endpoint reports usage.
fn print_context(history: &[Message], model: &str) {
    let (window, auto) = resolve_ctx_window(model);
    let total = session_tokens(history);
    let pct = (total as f64 / window as f64 * 100.0).min(100.0);

    let system = history
        .first()
        .filter(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    let sys_blocks = system_block_chars(system);
    let sys_tok: usize = sys_blocks.iter().map(|(_, c)| c / 4).sum();
    let schemas = agent::schema_overhead_tokens();

    // Everything after the system message, bucketed by role.
    let (mut user_tok, mut asst_tok, mut tool_tok) = (0usize, 0usize, 0usize);
    for m in history.iter().skip(1) {
        let t = agent::estimate_message_tokens(m);
        match m.role.as_str() {
            "assistant" => asst_tok += t,
            "tool" => tool_tok += t,
            _ => user_tok += t, // user turns + any stray system nudges
        }
    }
    let convo = user_tok + asst_tok + tool_tok;

    // One aligned row: label left-padded to a column, "~X.XK tok" right; sub-rows dimmed + indented.
    fn line(label: &str, tok: usize, depth: usize, dim: bool) -> String {
        let name = format!("{}{}", "  ".repeat(depth), label);
        let s = format!("{name:<26} {:>10}", format!("~{} tok", fmt_k(tok)));
        if dim {
            style(s).dim().to_string()
        } else {
            s
        }
    }

    tui::emit_line(&format!(
        "{}  {model} · window {}{}",
        style("📊 context breakdown")
            .color256(splash::ACCENT)
            .bold(),
        fmt_k(window),
        if auto { "" } else { " (est)" },
    ));
    tui::emit_line(&line("system prompt", sys_tok, 0, false));
    for (label, c) in &sys_blocks {
        if c / 4 > 0 {
            tui::emit_line(&line(label, c / 4, 1, true));
        }
    }
    tui::emit_line(&line("tool schemas", schemas, 0, false));
    tui::emit_line(&line("conversation", convo, 0, false));
    if convo > 0 {
        tui::emit_line(&line("user turns", user_tok, 1, true));
        tui::emit_line(&line("assistant turns", asst_tok, 1, true));
        tui::emit_line(&line("tool results", tool_tok, 1, true));
    }
    let bar = format!("{} {}", ctx_bar(pct), style(format!("{pct:.0}%")).dim());
    tui::emit_line(&format!(
        "{}  {} {bar}",
        style(format!("{:<26}", "total"))
            .color256(splash::ACCENT)
            .bold(),
        style(format!("~{} / {} tok", fmt_k(total), fmt_k(window))).color256(splash::ACCENT),
    ));
}

#[cfg(test)]
#[path = "tests/context_breakdown.rs"]
mod context_breakdown_tests;

/// Tell the user, unmistakably, when a turn ended for a reason that is NOT success.
///
/// The agent loop can return with the work unfinished or the tree broken, and in those cases the
/// model has usually ALREADY streamed a confident closing paragraph — so silence here means the
/// failure is indistinguishable from `Done`, and the post-turn passes go on to file it as a normal
/// episode and store it as a normal session. That is the one failure mode worth spending screen
/// space on: a wrong answer the user has no reason to doubt.
///
/// Each line names the recovery move, because the state differs: `VerificationFailed` means edits
/// LANDED and the checker never went green (so the tree is the thing to look at), while `MaxIters`
/// and `Divergence` mean the work simply stopped short (so continuing is the move). `Done` prints
/// nothing — the answer already speaks for itself. `Cancelled` / `AwaitingInput` never reach here:
/// their callers own dedicated arms upstream.
fn surface_abnormal_stop(outcome: &AgentOutcome) {
    let line = match &outcome.stop {
        StopReason::Done => return,
        StopReason::VerificationFailed => format!(
            "⚠ edits were made but verification never passed ({} steps). The tree is likely broken \
             — `/diff` to see what changed, `/rewind` to undo, or tell me to keep fixing.",
            outcome.iters
        ),
        // Reaching here now means the loop ALREADY granted itself every continuation it was allowed
        // (see `AgentConfig::max_continuations`) — so this is a genuinely long task, not the old
        // "cut off at step 50" case. Say that, rather than implying one more nudge would have done it.
        StopReason::MaxIters => format!(
            "⚠ ran out of step budget after {} steps, including the automatic continuations — the \
             task may be incomplete. Say \"continue\" to carry on from here.",
            outcome.iters
        ),
        // Both signature loops and evidence-flat exploration reach here. The final synthesis above
        // has already returned the best answer available; this line states why tool use stopped.
        StopReason::Divergence => format!(
            "⚠ stopped after {} steps: recent attempts added no new evidence. The answer above is the \
             best result from the established facts; say \"continue\" to try a different approach.",
            outcome.iters
        ),
        // Both have dedicated arms in every caller (Esc / `clarify` pause), so reaching this is a
        // wiring slip rather than a real state — still say something instead of swallowing it.
        StopReason::Cancelled => format!("⚠ stopped: cancelled after {} step(s).", outcome.iters),
        StopReason::AwaitingInput(q) => format!("❓ {q}"),
        // Only reachable if a wall-clock budget was set on this run (no top-level default), so name
        // the knob — otherwise the user cannot tell a deadline from a step limit or a crash.
        StopReason::Deadline => format!(
            "⚠ stopped: wall-clock budget reached after {} step(s) — the task may be incomplete. \
             Say \"continue\" to carry on, or raise AIZEN_SUBAGENT_WALL_SECS.",
            outcome.iters
        ),
    };
    let painted = theme::err(line).to_string();
    if tui::active() {
        tui::emit_line(&painted);
    } else {
        eprintln!("{painted}");
    }
}

/// Render a `clarify` question prominently and yield to the input box. `display` is the tool's
/// stored text: the question on the first line, any numbered options on the following lines.
/// Routes through `tui::emit_line` under the sticky TUI, else plain stdout — so the user just types
/// their answer next (it becomes the agent's next user turn). The dim `↳` hint sits below.
fn show_clarify(display: &str) {
    let mut lines = display.lines();
    let q = lines.next().unwrap_or("");
    let head = format!(
        "{} {}",
        style("❓").color256(splash::ACCENT).bold(),
        style(q).bold()
    );
    let opts: Vec<String> = lines
        .map(|l| style(l).color256(splash::ACCENT).to_string())
        .collect();
    let hint = style("↳ type your answer below to continue")
        .dim()
        .to_string();
    if tui::active() {
        tui::emit_line(&head);
        for o in &opts {
            tui::emit_line(o);
        }
        tui::emit_line(&hint);
    } else {
        println!("{head}");
        for o in &opts {
            println!("{o}");
        }
        println!("{hint}");
    }
}

/// What preprocessing a typed REPL line decided.
enum InputPre {
    /// A `#remember` / `!shell-escape` — handled inline, run NO agent turn.
    Handled,
    /// A normal message (its `@file` / inline `` !`cmd` `` refs expanded) → send as a chat turn.
    Send(String),
}

/// Cap shell-escape output so one chatty command can't flood the transcript.
const SHELL_ESCAPE_CAP: usize = 6000;

/// Preprocess a typed REPL line for the input-box affordances: `#text` captures a memory fact and
/// `!cmd` is a shell escape (both run NO turn); a normal message has its `@file` and inline
/// `` !`cmd` `` refs expanded. Output routes through `tui::emit_line` (works under the sticky TUI and
/// the plain REPL alike). Sync — every step (remember / classify / expand / run) is synchronous.
fn preprocess_input(line: &str) -> InputPre {
    let t = line.trim_start();
    // `#text` → remember a fact directly (the highest-confidence capture → straight into the store).
    if let Some(rest) = t.strip_prefix('#') {
        let text = rest.trim();
        if text.is_empty() {
            tui::emit_line(
                &style("# — type the fact after the # to remember it (this project's zone; `#global: …` for everywhere)")
                    .dim()
                    .to_string(),
            );
        } else {
            match memory::remember(text) {
                Ok(id) => tui::emit_line(
                    &style(format!("🧠 remembered ({id})"))
                        .color256(splash::ACCENT)
                        .to_string(),
                ),
                Err(e) => tui::emit_line(&format!("{} {e}", style("memory:").red())),
            }
        }
        return InputPre::Handled;
    }
    // `!cmd` → shell escape. The user typed it explicitly (like a terminal), so it runs without an
    // approval prompt — but the hard safety floor still refuses catastrophic commands.
    if let Some(rest) = t.strip_prefix('!') {
        let cmd = rest.trim();
        if cmd.is_empty() {
            tui::emit_line(
                &style("! — type a shell command after the !")
                    .dim()
                    .to_string(),
            );
            return InputPre::Handled;
        }
        match crate::agent::cmd_guard::classify(cmd) {
            crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                tui::emit_line(&format!(
                    "{} blocked by the safety floor: {reason}",
                    theme::warn("✗")
                ));
            }
            _ => {
                let out = run_shell_escape(cmd);
                tui::emit_line(&format!(
                    "{} {cmd}\n{out}",
                    style("$").color256(splash::ACCENT)
                ));
            }
        }
        return InputPre::Handled;
    }
    // A normal message → expand `@file` + inline `` !`cmd` `` before it's sent to the agent.
    match commands::expand_refs(line) {
        Ok(expanded) => InputPre::Send(expanded),
        Err(e) => {
            tui::emit_line(&format!("{} {e}", style("input:").red()));
            InputPre::Handled // a ref failed (e.g. a blocked `!`cmd``) → don't send a half-expanded turn
        }
    }
}

/// Run a user-typed `!cmd` shell escape in the working dir, capturing stdout+stderr (lossy-decode +
/// `chcp 65001` like `shell_run` so non-English Windows output isn't dropped), capped for display.
fn run_shell_escape(command: &str) -> String {
    run_shell_escape_in(command, None)
}

/// As `run_shell_escape`, but in an EXPLICIT directory. The hostbot daemon passes its lane's cwd:
/// several bots share one process, so `/sh` must run where that bot was told to work, not wherever
/// the process happens to be. `None` ⇒ inherit the process cwd (the REPL's `!cmd`).
fn run_shell_escape_in(command: &str, dir: Option<&std::path::Path>) -> String {
    use std::process::Command;
    use std::time::Duration;
    /// A `!cmd` escape runs on the REPL's own thread, so an unbounded wait freezes the entire UI —
    /// not one tool call. `Command::output()` has no deadline (it waits for pipe EOF, which a
    /// grandchild outliving its wrapper never delivers), so this goes through the bounded helper.
    /// Generous, because the user typed this command deliberately and is watching it.
    const ESCAPE_TIMEOUT: Duration = Duration::from_secs(120);
    const ESCAPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(format!("chcp 65001>nul & {command}"));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    match core::proctree::output_bounded(&mut cmd, ESCAPE_TIMEOUT, ESCAPE_DRAIN_GRACE) {
        Ok(o) => {
            let mut s = o.stdout;
            if !o.stderr.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&o.stderr);
            }
            if o.output_truncated {
                s.push_str("\n…[output cut: a surviving child process still held the pipe]");
            }
            let s = s.trim_end().to_string();
            let s = if s.chars().count() > SHELL_ESCAPE_CAP {
                let head: String = s.chars().take(SHELL_ESCAPE_CAP).collect();
                format!("{head}\n…[output truncated]")
            } else {
                s
            };
            if o.timed_out {
                return format!(
                    "[timed out after {}s — killed the whole process tree]\n{s}",
                    ESCAPE_TIMEOUT.as_secs()
                )
                .trim_end()
                .to_string();
            }
            if s.is_empty() {
                format!("(exit {}, no output)", o.code.unwrap_or(-1))
            } else {
                s
            }
        }
        Err(e) => format!("[failed to run: {e}]"),
    }
}

/// The auto-compact threshold as a percent of the context window. `0` ⇒ disabled; `None` ⇒ 80%.
fn compact_threshold_pct() -> u8 {
    cli_config::load().compact_threshold_pct.unwrap_or(80)
}

/// Whether the REPL auto-distills completed multi-step tasks into skills. `None` ⇒ default ON.
fn auto_skill_learn_enabled() -> bool {
    cli_config::load().auto_skill_learn.unwrap_or(true)
}

/// Effective unified approval level; `AIZEN_YES` forces yolo without changing the saved preference.
fn approval_mode() -> ApprovalMode {
    cli_config::approval_mode()
}

/// Arm the LSP manager once per process (default ON, lazy spawn). Safe to call every turn:
/// - first call enables the runtime (no language server process until a query needs one);
/// - later calls are no-ops, so a mid-session `/lsp off` stays off until the user runs `/lsp on`.
/// Always refreshes request timeout + edit-feedback from config.
fn arm_lsp_session() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ARMED: AtomicBool = AtomicBool::new(false);
    crate::agent::lsp::LSP.set_request_timeout(AgentConfig::default().lsp_request_timeout_secs);
    crate::agent::lsp::LSP
        .set_edit_feedback(cli_config::load().lsp_edit_diagnostics.unwrap_or(true));
    if !ARMED.swap(true, Ordering::Relaxed) {
        let _ = crate::agent::lsp::LSP.enable();
    }
}

/// Whether an active persona evolves (records episodes + reflects). `None` ⇒ default ON.
fn persona_evolve_enabled() -> bool {
    cli_config::load().persona_evolve.unwrap_or(true)
}

/// Pull the first top-level JSON object out of a model reply (tolerating ```json fences / prose).
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// One-line status above the prompt: model · context-fill bar+% · approx tokens · turns · telegram.
fn print_status_line(history: &[Message], model: &str) {
    let toks = session_tokens(history);
    let turns = history.iter().filter(|m| m.role == "user").count();
    let tg = if telegram::is_configured() {
        "  ·  📱 telegram"
    } else {
        ""
    };
    let approval = approval_mode();
    let mode = if cli_config::ultimate_enabled() {
        format!("  ·  {}", style("✦ ultimate").color256(theme::WARN).bold())
    } else if approval == ApprovalMode::Yolo {
        format!("  ·  {}", style("⚡ yolo").color256(theme::WARN))
    } else if approval == ApprovalMode::Smart {
        format!("  ·  {}", style("◆ smart").color256(theme::ACCENT_DIM))
    } else {
        String::new()
    };
    let (window, auto) = resolve_ctx_window(model);
    let pct = (toks as f64 / window as f64 * 100.0).min(100.0);
    let toklabel = if toks >= 1000 {
        format!("~{:.1}K", toks as f64 / 1000.0)
    } else {
        format!("~{toks}")
    };
    let winlabel = if window >= 1000 {
        format!("{}K", window / 1000)
    } else {
        window.to_string()
    };
    let tag = if auto { "ctx" } else { "ctx·est" }; // est = name-heuristic, provider didn't report it
    let ctx = format!(
        "{} {}",
        ctx_bar(pct),
        style(format!("{pct:.0}% {tag}")).dim()
    );
    // Auto-compact trigger level, plus how many times this session has actually compacted so far
    // (P-ctx3, read from the queryable boundary marker). `⊟ 80%` → `⊟ 80% ×2` after two compactions.
    let ac = match compact_threshold_pct() {
        0 => String::new(),
        t => {
            let n = agent::compact::compaction_count(history);
            let count = if n > 0 {
                format!(" ×{n}")
            } else {
                String::new()
            };
            style(format!("  ·  ⊟ {t}%{count}")).dim().to_string()
        }
    };
    let cache = cache_hit_label()
        .map(|s| style(format!("  ·  {s}")).dim().to_string())
        .unwrap_or_default();
    let rest = style(format!(
        "{}{model}  ·  {toklabel}/{winlabel} tok  ·  {turns} turns{tg}",
        icons::g(icons::spark())
    ))
    .dim();
    // emit_line routes into the sticky scroll region (above the box) when active, else plain stdout.
    tui::emit_line(&format!("\n{rest}  ·  {ctx}{ac}{cache}{mode}"));
}

/// How many trailing user turns to keep verbatim when compacting (the rest is summarized).
const COMPACT_KEEP_TURNS: usize = agent::compact::KEEP_TURNS;

/// Shorten for a human-facing list: keep the head, mark the cut with an ellipsis.
///
/// NOT [`agent::compact::truncate_chars`], which appends `[+N chars]` — that suffix exists so a
/// MODEL reading a truncated tool result knows content was withheld. In a listing it is noise the
/// reader can't act on, and it costs the width the description needed.
fn elide(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// Render conversation messages into a compact transcript (delegates to the shared compaction core).
fn render_transcript(msgs: &[Message]) -> String {
    agent::compact::render_transcript(msgs)
}

/// Summarize older turns to free context. Thin wrapper over [`agent::compact::compact_history`] that
/// supplies a NON-streaming summarize closure over this session's endpoint. Returns
/// (tokens_before, tokens_after). Same core the agent loop uses, so the REPL and `aizen serve` compact
/// identically.
async fn compact_history(
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) -> Result<(usize, usize)> {
    let sum_ep = summarizer_endpoint(base, key, model);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    agent::compact::compact_history(history, summarize, COMPACT_KEEP_TURNS).await
}

/// `/compact` — resolve the endpoint, then summarize older turns now (manual compaction).
async fn compact_now(history: &mut Vec<Message>) -> Result<(usize, usize)> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    compact_history(history, &http, &base, &key, &model).await
}

/// `/handoff` — one goal-conditioned extraction call over the current history (routed through the
/// summarizer role, like compaction). Returns the extraction; the caller rebuilds the thread.
async fn handoff_now(history: &[Message], goal: &str) -> Result<String> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    if history.len() < 2 {
        anyhow::bail!("nothing to hand off yet — the conversation is empty");
    }
    let ep = summarizer_endpoint(&base, &key, &model);
    let prompt = agent::compact::handoff_prompt(history, goal);
    let summary = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &prompt, &[])
        .await?
        .content
        .unwrap_or_default();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty handoff summary");
    }
    Ok(summary.trim().to_string())
}

/// `/memory <sub>` — the in-REPL view of the same store the agent writes through, so the user can
/// audit and correct it without dropping to the CLI. Sub-commands mirror `aizen memory <sub>` 1:1
/// (same functions, same ids) rather than reimplementing a second, drifting surface.
///
/// `forget` here is the SOFT delete (archive → restorable); hard `purge` is CLI-only on purpose, so
/// nothing typed mid-chat can destroy a fact irreversibly.
fn slash_memory(arg: &str) -> Result<()> {
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((s, r)) => (s.trim(), r.trim()),
        None => (arg.trim(), ""),
    };
    match sub {
        // Bare `/memory` keeps its old meaning (the rolled-up profile).
        "" => memory::cmd_profile(false),
        "list" | "ls" => memory::cmd_list(if rest.is_empty() { None } else { Some(rest) }),
        "show" | "cat" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory show <id>  (ids from `/memory list`)");
            }
            memory::cmd_show(rest)
        }
        "remember" | "add" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory remember <fact>");
            }
            let id = memory::remember(rest)?;
            tui::emit_line(
                &style(format!("{}remembered ({id})", icons::g(icons::learned())))
                    .color256(splash::ACCENT)
                    .to_string(),
            );
            Ok(())
        }
        "edit" | "update" => {
            // `/memory edit <id> <new body>` — the common correction. Field-by-field editing
            // (description/type/scope) stays on the CLI, which has real flags for it.
            let (id, body) = rest
                .split_once(char::is_whitespace)
                .map(|(i, b)| (i.trim(), b.trim()))
                .unwrap_or((rest, ""));
            if id.is_empty() || body.is_empty() {
                anyhow::bail!("usage: /memory edit <id> <corrected fact>  (field flags: `aizen memory edit --help`)");
            }
            memory::cmd_edit(id, None, None, None, Some(body.to_string()), None)
        }
        "forget" | "rm" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory forget <id>  (archived, not erased — restorable)");
            }
            memory::cmd_forget(rest)
        }
        "archive" => memory::cmd_archive_list(),
        // The review queue is where mid-confidence learned facts wait for a human. It had a CLI but
        // no REPL door, so the queue only ever grew — 29 items deep on this machine before anyone
        // could see them. Same functions as `aizen memory review`, so the two can't drift.
        "review" | "queue" => {
            let (op, key) = match rest.split_once(char::is_whitespace) {
                Some((o, k)) => (o.trim(), k.trim()),
                None => (rest, ""),
            };
            match op {
                "" => memory::cmd_review(None, None, false),
                "promote" | "keep" | "accept" => {
                    if key.is_empty() {
                        anyhow::bail!("usage: /memory review promote <id>  (ids from `/memory review`)");
                    }
                    memory::cmd_review(Some(key.to_string()), None, false)
                }
                "drop" | "reject" => {
                    if key.is_empty() {
                        anyhow::bail!("usage: /memory review drop <id>  (ids from `/memory review`)");
                    }
                    memory::cmd_review(None, Some(key.to_string()), false)
                }
                "clear" => memory::cmd_review(None, None, true),
                other => anyhow::bail!(
                    "unknown: /memory review {other}  (try: review | review promote <id> | review drop <id> | review clear)"
                ),
            }
        }
        "restore" => {
            if rest.is_empty() {
                anyhow::bail!(
                    "usage: /memory restore <id> [--as <new-id>]  (ids from `/memory archive`)"
                );
            }
            // `--as` has to be reachable from here too: a collision makes plain `restore` fail, and
            // without the escape hatch in the REPL the only way out would be to leave the REPL.
            let (id, as_id) = match rest.split_once("--as") {
                Some((a, b)) => (a.trim(), Some(b.trim()).filter(|s| !s.is_empty())),
                None => (rest, None),
            };
            if id.is_empty() {
                anyhow::bail!("usage: /memory restore <id> [--as <new-id>]");
            }
            memory::cmd_restore(id, as_id)
        }
        "profile" => memory::cmd_profile(false),
        "style" => memory::cmd_style(),
        "frozen" | "core" => memory::cmd_frozen(false),
        // Anything else is treated as a search query, which is what `/memory <words>` always did.
        _ => memory::cmd_search(arg, 5, None, None, None),
    }
}

enum SlashOutcome {
    Continue,
    Quit,
    /// A custom command expanded to this prompt — feed it through the normal chat path.
    Submit(String),
}

/// Bare `/` → an arrow-key picker over the slash commands; runs the chosen one (default args).
/// Built-ins and user-defined custom commands both come from the shared [`crate::features::slash`]
/// catalog, so the picker, the live palette, and `/help` can never drift apart.
async fn slash_menu(history: &mut Vec<Message>, model_label: &mut String) -> SlashOutcome {
    let catalog = crate::features::slash::list();
    let items: Vec<String> = catalog
        .iter()
        .map(|c| {
            let hint = if c.argument_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", c.argument_hint)
            };
            let icon = icons::g(icons::slash(if c.custom { "commands" } else { &c.name }));
            format!("{icon}/{}{hint}  —  {}", c.name, c.description)
        })
        .collect();
    let theme = ui_theme();
    match Select::with_theme(&theme)
        .with_prompt("slash command")
        .items(&items)
        .default(0)
        .interact_opt()
    {
        // Every entry (built-in or custom) dispatches by name through the one `handle_slash` path.
        Ok(Some(i)) => handle_slash(&catalog[i].name, history, model_label).await,
        _ => SlashOutcome::Continue, // Esc / error → back to the prompt
    }
}

/// Slash commands that drive the terminal directly (dialoguer menus, the Telegram daemon) and so
/// need the sticky box SUSPENDED. Everything else is pure-print: it runs with the box still up and
/// its `tui::emit_line` output flows into the scroll region (so short output isn't painted over).
///
/// Delegates to the ONE shared table in `tui`. This used to be a second, independently maintained
/// list, and the two had drifted: this copy matched whole command names, so `/timeline pick` and
/// `/tools menu` opened a dialoguer menu without suspending the box, while `/memory` (pure-print)
/// was suspended for nothing. Anything that owns stdin must appear in exactly one place.
fn slash_is_interactive(cmd: &str) -> bool {
    tui::slash_takes_stdin(cmd)
}

async fn slash_tools(_arg: &str) {
    tui::emit_line(&agent::toolsets::format_config_status());
}

/// `/workflows` — live multi-agent activity. In retained mode the panel REFRESHES itself (elapsed
/// times tick while you watch); on a pipe/CI it degrades to a single printed snapshot.
///
/// `/workflows stop <id>` cancels one running row without touching the rest of the turn: Esc is
/// all-or-nothing (it cancels the whole turn), so a fan-out with one child stuck behind a slow model
/// call previously left the user no choice but to kill everything.
async fn slash_workflows(arg: &str) {
    // Shared with the input thread's mid-turn fast path, so an idle stop and a mid-turn stop cannot
    // report the same action differently.
    if let Some(note) = agent::orchestration::try_stop_command(arg) {
        tui::emit_line(&theme::muted(note).to_string());
        return;
    }
    if !tui::retained_overlay_open_live("Activity", agent::orchestration::format_status) {
        tui::emit_line(&agent::orchestration::format_status());
    }
}

/// `/init` — build (or incrementally refresh) the per-repo codebase index that powers
/// `codebase_search` + automatic per-turn retrieval. `--force`/`-f` rebuilds from scratch;
/// `--status`/`-s` shows the current index without scanning. Esc cancels a running scan cleanly
/// (the existing index is left untouched). The scan runs on a blocking thread so the REPL stays
/// responsive; progress is reported by phase (scan → chunk → build), never one line per file.
async fn slash_init(arg: &str) {
    use crate::agent::codebase;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let flags: Vec<String> = arg
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let want = |names: &[&str]| flags.iter().any(|f| names.contains(&f.as_str()));

    // `--status`: print the current index state, no scan.
    if want(&["--status", "-s", "status"]) {
        match codebase::load() {
            Some(idx) => {
                let ago = fmt_time_ago(idx.built_unix);
                tui::emit_line(&format!(
                    "{} {} file(s), {} chunk(s) — indexed {}",
                    style("✓ codebase index:").color256(splash::ACCENT),
                    idx.files.len(),
                    idx.chunks.len(),
                    ago
                ));
                let summary = codebase::analysis_summary(&idx.analysis);
                if !summary.trim().is_empty() {
                    tui::emit_line(&style(summary).dim().to_string());
                }
            }
            None => tui::emit_line(
                &style("no codebase index yet — run /init to build it")
                    .dim()
                    .to_string(),
            ),
        }
        return;
    }

    let force = want(&["--force", "-f", "force", "rebuild"]);
    let incremental = !force;

    // Arm a cancel token for this scan so Esc aborts it (the input thread calls request_cancel).
    let cancel = crate::core::cancel::TurnCancel::new();
    tui::arm_cancel(cancel.clone());
    tui::emit_line(
        &style(if force {
            "rebuilding codebase index…"
        } else {
            "indexing codebase…"
        })
        .dim()
        .to_string(),
    );

    // Phase progress, decile-throttled so a large scan reports ~10 lines, not one per file.
    let last_decile = std::sync::Arc::new(AtomicUsize::new(usize::MAX));
    let ld = last_decile.clone();
    let progress = move |phase: codebase::Phase| match phase {
        codebase::Phase::Scanning { done, total } => {
            if total == 0 {
                return;
            }
            let decile = done * 10 / total.max(1);
            if ld.swap(decile, Ordering::Relaxed) != decile {
                tui::emit_line(
                    &style(format!("  scanning… {}%", decile * 10))
                        .dim()
                        .to_string(),
                );
            }
        }
        codebase::Phase::Chunking => {
            tui::emit_line(&style("  chunking symbols…").dim().to_string())
        }
        codebase::Phase::Building => tui::emit_line(&style("  building index…").dim().to_string()),
    };

    let cancel_for_task = cancel.clone();
    let result = tokio::task::spawn_blocking(move || {
        codebase::build_index(incremental, Some(&cancel_for_task), &progress)
    })
    .await;
    tui::disarm_cancel(&cancel);

    match result {
        Ok(Ok(stats)) => {
            let mut parts = vec![
                format!("{} file(s)", stats.indexed),
                format!("{} chunk(s)", stats.chunks),
            ];
            if stats.reused > 0 {
                parts.push(format!("{} reused", stats.reused));
            }
            if stats.added > 0 {
                parts.push(format!("{} updated", stats.added));
            }
            if stats.removed > 0 {
                parts.push(format!("{} removed", stats.removed));
            }
            tui::emit_line(&format!(
                "{} {} in {}ms",
                style("✓ codebase indexed:").color256(splash::ACCENT),
                parts.join(", "),
                stats.elapsed_ms
            ));
            // Sensitivity / skip accounting — surfaced so the user knows coverage + that secrets
            // were protected, without ever printing a path or a secret value.
            let mut notes = Vec::new();
            if stats.sensitive > 0 {
                notes.push(format!(
                    "{} sensitive file(s) stored path-only",
                    stats.sensitive
                ));
            }
            if stats.redacted > 0 {
                notes.push(format!("{} file(s) had secrets redacted", stats.redacted));
            }
            if stats.skipped_large > 0 {
                notes.push(format!("{} oversized skipped", stats.skipped_large));
            }
            if stats.skipped_binary > 0 {
                notes.push(format!("{} binary skipped", stats.skipped_binary));
            }
            if stats.capped {
                notes.push("scan hit the file cap (coverage bounded)".to_string());
            }
            if !notes.is_empty() {
                tui::emit_line(&style(format!("  {}", notes.join(" · "))).dim().to_string());
            }
            let summary = codebase::analysis_summary(
                &codebase::load().map(|i| i.analysis).unwrap_or_default(),
            );
            if !summary.trim().is_empty() {
                tui::emit_line(&style(summary).dim().to_string());
            }
        }
        Ok(Err(e)) => {
            // A cancel is a clean, expected outcome — show it calmly, not as a hard error.
            let msg = e.to_string();
            if msg.contains("cancelled") {
                tui::emit_line(
                    &style("/init cancelled — the existing index was left unchanged")
                        .color256(theme::WARN)
                        .to_string(),
                );
            } else {
                tui::emit_line(&format!("{} {msg}", style("/init:").red()));
            }
        }
        Err(e) => tui::emit_line(&format!("{} scan task failed: {e}", style("/init:").red())),
    }
}

/// Compact "N ago" for a Unix-seconds timestamp (for `/init --status`).
pub(crate) fn fmt_time_ago(built_unix: u64) -> String {
    let now = chrono::Utc::now().timestamp() as u64;
    if built_unix == 0 || built_unix > now {
        return "just now".to_string();
    }
    let secs = now - built_unix;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} hour(s) ago", secs / 3600)
    } else {
        format!("{} day(s) ago", secs / 86_400)
    }
}

/// `/agents` — list installed specialists with their effective provider/model assignment. The normal
/// write path is `/agents set-provider`; legacy card `set-model` remains available for compatibility.
fn slash_agents(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "set-provider" | "provider" => {
            let mut rp = rest.split_whitespace();
            let name = rp.next().unwrap_or("");
            let provider = rp.next().unwrap_or("");
            let model = rp.next();
            if name.is_empty() || provider.is_empty() {
                tui::emit_line(&style("usage: /agents set-provider <agent> <provider> [model]   ·   clear: /agents set-provider <agent> clear").dim().to_string());
                return;
            }
            let mut cfg = cli_config::load();
            let result = if provider.eq_ignore_ascii_case("clear") || provider == "-" {
                cfg.set_agent_route(name, None, None)
            } else {
                cfg.set_agent_route(name, Some(provider.to_string()), model.map(str::to_string))
            };
            match result.and_then(|_| cli_config::save(&cfg)) {
                Ok(()) => tui::emit_line(
                    &style(format!("updated provider assignment for '{name}'"))
                        .color256(theme::OK)
                        .to_string(),
                ),
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("agents:").red())),
            }
        }
        "set-model" | "model" => {
            let mut rp = rest.splitn(2, char::is_whitespace);
            let name = rp.next().unwrap_or("").trim();
            let model = rp.next().unwrap_or("").trim();
            if name.is_empty() {
                tui::emit_line(&style("usage: /agents set-model <name> <model>   (omit <model> or pass `clear` to remove the pin)").dim().to_string());
                return;
            }
            let clear = model.is_empty() || model.eq_ignore_ascii_case("clear") || model == "-";
            let value = if clear { None } else { Some(model.to_string()) };
            match agents::set_model(name, value.as_deref()) {
                Ok(path) => {
                    let msg = match &value {
                        Some(m) => format!("pinned '{name}' → model {m}  ({})", path.display()),
                        None => format!("cleared model pin on '{name}'  ({})", path.display()),
                    };
                    tui::emit_line(&style(msg).color256(theme::OK).to_string());
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("agents:").red())),
            }
        }
        "" | "list" => {
            let all = agents::list();
            if all.is_empty() {
                tui::emit_line(&style("no specialist agents installed — `aizen agents install msitarzewski/agency-agents`").dim().to_string());
                return;
            }
            let enabled = agents::enabled_set();
            let mut out = String::from("specialist agents (● pinned to <agents> index / ○ not):\n");
            let cfg = cli_config::load();
            for def in &all {
                let slug = def.slug();
                let pin = enabled.as_ref().map(|s| s.contains(&slug)).unwrap_or(true);
                let mark = if pin { "●" } else { "○" };
                let route = cfg.agent_route(&slug);
                let provider = route
                    .and_then(|r| r.provider.as_deref())
                    .unwrap_or("inherit");
                let model = route
                    .and_then(|r| r.model.as_deref())
                    .or(def.model.as_deref())
                    .unwrap_or("default");
                out.push_str(&format!(
                    "  {mark} {:<24} provider: {provider} · model: {model}\n",
                    slug
                ));
            }
            out.push_str("\nset a provider: /agents set-provider <agent> <provider> [model]   ·   clear: ... <agent> clear");
            tui::emit_line(&out.trim_end().to_string());
        }
        other => {
            tui::emit_line(&style(format!("unknown /agents subcommand '{other}' — try /agents or /agents set-provider <agent> <provider> [model]")).dim().to_string());
        }
    }
}

/// Rows for `/team status` — every aizen session registered in this repository.
///
/// The point of this table is the LAST two columns: who is still running, and which files each
/// window changed. `git diff` alone cannot answer either question once two windows share a tree.
fn team_status_lines() -> Vec<String> {
    let sessions = coop::list();
    if sessions.is_empty() {
        return vec![style(
            "no aizen sessions registered here yet — this window registers itself on start",
        )
        .dim()
        .to_string()];
    }
    let mut out = vec![format!(
        "{}  {} session(s) in this repository",
        style("⚑ team").color256(splash::ACCENT).bold(),
        sessions.len()
    )];
    for (i, v) in sessions.iter().enumerate() {
        let m = &v.manifest;
        let state = v.effective_state();
        let color = match state {
            "working" | "awaiting-approval" => theme::LINK,
            "done" | "committed" | "finished" => theme::OK,
            "abandoned" | "failed" => theme::WARN,
            _ => theme::MUTED,
        };
        let mine = if v.is_self { " ●" } else { "" };
        out.push(format!(
            "  {:>2}. {}{mine}  {}  {}",
            i + 1,
            style(&m.session_id).bold(),
            style(format!("[{state}]")).color256(color),
            style(format!(
                "{} file(s) · {} turn(s) · {}",
                m.files.len(),
                m.turns,
                relative_age_secs(now_unix_secs().saturating_sub(m.updated_unix))
            ))
            .dim(),
        ));
        if !m.task.is_empty() {
            out.push(format!(
                "      {}",
                style(&m.task).color256(theme::ACCENT_DIM)
            ));
        }
        let wt = v.worktree_label();
        if !m.branch.as_deref().unwrap_or("").is_empty() || !wt.is_empty() {
            out.push(
                style(format!(
                    "      {} · branch {}",
                    wt,
                    m.branch.as_deref().unwrap_or("(detached)")
                ))
                .dim()
                .to_string(),
            );
        }
        if !v.overlapping.is_empty() {
            out.push(
                style(format!(
                    "      ⚠ shares {} file(s) with another session: {}",
                    v.overlapping.len(),
                    v.overlapping.join(", ")
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
        if let Some(reason) = &m.degraded {
            out.push(
                style(format!("      ⚠ no per-session diff: {reason}"))
                    .color256(theme::WARN)
                    .to_string(),
            );
        }
        if m.truncated_files > 0 {
            out.push(
                style(format!(
                    "      ⚠ {} path(s) beyond the tracking ceiling are not recorded",
                    m.truncated_files
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
    }
    out.push(
        style(
            "/team diff <n> [-p] · /team files <n> · /team claims · /team commit <n> · /team done",
        )
        .dim()
        .to_string(),
    );
    out
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact age from an already-computed elapsed span. `fmt_time_ago` takes a timestamp and reads the
/// clock itself; the `/team` tables have many rows against ONE `now`, so they subtract once.
fn relative_age_secs(secs: u64) -> String {
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `/team` — read and act on what the OTHER aizen windows in this repository are doing.
async fn slash_team(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "" | "status" | "ls" | "list" => {
            for line in team_status_lines() {
                tui::emit_line(&line);
            }
        }
        "task" => {
            if rest.is_empty() {
                tui::emit_line(
                    &style("usage: /team task <what this window is working on>")
                        .dim()
                        .to_string(),
                );
                return;
            }
            coop::set_task(rest);
            tui::emit_line(
                &style(format!("this session's task: {rest}"))
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        "done" => {
            coop::set_state(coop::SessionState::Done);
            tui::emit_line(
                &style("marked this session done — another window can now commit its work")
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        "files" => match coop::resolve(rest) {
            Ok(view) => {
                let paths = coop::session_paths(&view);
                if paths.is_empty() {
                    tui::emit_line(
                        &style(format!(
                            "{} has no recorded file changes",
                            view.manifest.session_id
                        ))
                        .dim()
                        .to_string(),
                    );
                    return;
                }
                tui::emit_line(&format!(
                    "{} {} file(s) changed by {}",
                    style("⚑").color256(splash::ACCENT),
                    paths.len(),
                    style(&view.manifest.session_id).bold()
                ));
                for f in &view.manifest.files {
                    tui::emit_line(&format!(
                        "  {} {}  {}",
                        f.status,
                        f.path,
                        style(format!("from checkpoint #{}", f.base)).dim()
                    ));
                }
            }
            Err(e) => tui::emit_line(&format!("{} {e:#}", style("team:").red())),
        },
        "diff" => {
            let mut patch = false;
            let mut who = String::new();
            for tok in rest.split_whitespace() {
                match tok {
                    "-p" | "--patch" => patch = true,
                    _ => {
                        if who.is_empty() {
                            who = tok.to_string();
                        }
                    }
                }
            }
            let view = match coop::resolve(&who) {
                Ok(v) => v,
                Err(e) => {
                    tui::emit_line(&format!("{} {e:#}", style("team:").red()));
                    return;
                }
            };
            match coop::session_diff(&view, patch) {
                Ok(reports) if reports.is_empty() => tui::emit_line(
                    &style(format!(
                        "{} has no changes still present in the working tree",
                        view.manifest.session_id
                    ))
                    .dim()
                    .to_string(),
                ),
                Ok(reports) => {
                    tui::emit_line(&format!(
                        "{} changes attributed to {}",
                        style("⚑").color256(splash::ACCENT),
                        style(&view.manifest.session_id).bold()
                    ));
                    for report in &reports {
                        for line in diff_lines(report, "/team diff <n>") {
                            tui::emit_line(&line);
                        }
                    }
                    if !view.overlapping.is_empty() {
                        tui::emit_line(
                            &style(format!(
                                "⚠ {} file(s) were also changed by another session — the diff above \
                                 cannot separate their lines: {}",
                                view.overlapping.len(),
                                view.overlapping.join(", ")
                            ))
                            .color256(theme::WARN)
                            .to_string(),
                        );
                    }
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("team diff:").red())),
            }
        }
        "claims" => {
            let claims = coop::claims();
            if claims.is_empty() {
                tui::emit_line(&style("no files are claimed yet").dim().to_string());
                return;
            }
            tui::emit_line(&format!(
                "{} {} claimed file(s) — most recent writer owns the claim",
                style("⚑ claims").color256(splash::ACCENT).bold(),
                claims.len()
            ));
            for (path, claim) in claims.iter().take(200) {
                tui::emit_line(&format!(
                    "  {path}  {}",
                    style(format!(
                        "{} · {}",
                        claim.session_id,
                        relative_age_secs(now_unix_secs().saturating_sub(claim.unix))
                    ))
                    .dim()
                ));
            }
            let overlaps = coop::overlaps();
            if !overlaps.is_empty() {
                tui::emit_line(
                    &style(format!("⚠ {} overlapping file(s):", overlaps.len()))
                        .color256(theme::WARN)
                        .to_string(),
                );
                for o in overlaps.iter().take(100) {
                    tui::emit_line(&format!("  {}  {} → {}", o.path, o.first, o.second));
                }
            }
        }
        "commit" => slash_team_commit(rest).await,
        other => tui::emit_line(
            &style(format!(
                "unknown /team subcommand '{other}' — status · task · done · files · diff · claims · commit"
            ))
            .dim()
            .to_string(),
        ),
    }
}

/// `/team commit <session> [-m msg] [--verify] [--force] [--dry-run]`.
///
/// Staging is derived from the session's ledger, so the coordinator commits ONE window's work rather
/// than whatever happens to be in the tree. The commit itself is confirmed interactively — it is the
/// one irreversible step here.
async fn slash_team_commit(rest: &str) {
    let mut who = String::new();
    let mut message = String::new();
    let mut verify = false;
    let mut force = false;
    let mut dry_run = false;
    let mut expect_message = false;
    for tok in rest.split_whitespace() {
        if expect_message {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(tok);
            continue;
        }
        match tok {
            "-m" | "--message" => expect_message = true,
            "--verify" => verify = true,
            "--force" => force = true,
            "--dry-run" | "-n" => dry_run = true,
            _ if who.is_empty() => who = tok.to_string(),
            _ => {}
        }
    }
    let view = match coop::resolve(&who) {
        Ok(v) => v,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    let plan = match coop::plan_commit(&view) {
        Ok(p) => p,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    tui::emit_line(&format!(
        "{} {} file(s) from {}",
        style("⚑ commit plan").color256(splash::ACCENT).bold(),
        plan.paths.len(),
        style(&plan.session_id).bold()
    ));
    for p in plan.paths.iter().take(100) {
        tui::emit_line(&format!("  {p}"));
    }
    if plan.paths.len() > 100 {
        tui::emit_line(
            &style(format!("  … {} more", plan.paths.len() - 100))
                .dim()
                .to_string(),
        );
    }
    if !plan.shared_paths.is_empty() {
        tui::emit_line(
            &style(format!(
                "⚠ {} of these file(s) were also changed by another session; committing them takes \
                 that work along — git cannot split one file by author: {}",
                plan.shared_paths.len(),
                plan.shared_paths.join(", ")
            ))
            .color256(theme::WARN)
            .to_string(),
        );
    }
    if !plan.blockers.is_empty() {
        for b in &plan.blockers {
            tui::emit_line(&style(format!("⚠ {b}")).color256(theme::WARN).to_string());
        }
        if !force {
            tui::emit_line(
                &style("nothing was staged. Re-run with --force to proceed anyway.")
                    .dim()
                    .to_string(),
            );
            return;
        }
    }
    let review = match coop::stage_plan(&plan) {
        Ok(s) => s,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    for line in review.stat.lines() {
        tui::emit_line(&format!("  {line}"));
    }
    if !review.separated.is_empty() {
        tui::emit_line(
            &style(format!(
                "↔ {} shared file(s) were separated: the commit holds only this session's version, \
                 while the working tree keeps both sessions' edits — {}",
                review.separated.len(),
                review.separated.join(", ")
            ))
            .color256(splash::ACCENT)
            .to_string(),
        );
    }
    if verify {
        match crate::agent::verify_gate::run_verify_gate(&plan.root, 300).await {
            None => tui::emit_line(
                &style("verify: no known verify command for this project — skipped")
                    .dim()
                    .to_string(),
            ),
            Some(r) if r.passed => tui::emit_line(
                &style(format!("verify: {} passed", r.command))
                    .color256(theme::OK)
                    .to_string(),
            ),
            Some(r) => {
                tui::emit_line(
                    &style(crate::agent::verify_gate::format_gate_failure(&r))
                        .color256(theme::WARN)
                        .to_string(),
                );
                let _ = coop::unstage_plan(&plan);
                tui::emit_line(
                    &style("unstaged the plan — the working tree was not touched")
                        .dim()
                        .to_string(),
                );
                return;
            }
        }
    }
    if dry_run {
        let _ = coop::unstage_plan(&plan);
        tui::emit_line(
            &style("dry run — unstaged again, nothing was committed")
                .dim()
                .to_string(),
        );
        return;
    }
    let msg = if message.trim().is_empty() {
        let task = view.manifest.task.trim();
        if task.is_empty() {
            format!("aizen session {}", plan.session_id)
        } else {
            task.to_string()
        }
    } else {
        message.trim().to_string()
    };
    let status = tui::current_status();
    tui::suspend();
    let go = Confirm::with_theme(&ui_theme())
        .with_prompt(format!("Commit {} file(s) as “{msg}”?", plan.paths.len()))
        .default(false)
        .interact()
        .unwrap_or(false);
    tui::resume(&status);
    if !go {
        let _ = coop::unstage_plan(&plan);
        tui::emit_line(
            &style("cancelled — unstaged, working tree untouched")
                .dim()
                .to_string(),
        );
        return;
    }
    match coop::commit_staged(&plan, &msg, &review) {
        Ok(out) => {
            for line in out.lines().take(6) {
                tui::emit_line(&format!("  {line}"));
            }
            tui::emit_line(
                &style(format!("committed {}'s work", plan.session_id))
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        Err(e) => tui::emit_line(&format!("{} {e:#}", style("team commit:").red())),
    }
}

/// `/work` — isolated worktree mode: one aizen window per linked worktree + branch.
fn slash_work(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "" | "list" | "ls" => match coop::work_list() {
            Ok(list) if list.is_empty() => tui::emit_line(
                &style("no aizen worktrees — create one with /work new <name>")
                    .dim()
                    .to_string(),
            ),
            Ok(list) => {
                tui::emit_line(&format!(
                    "{}  {} worktree(s)",
                    style("⚑ work").color256(splash::ACCENT).bold(),
                    list.len()
                ));
                for w in &list {
                    let flags = coop::work_remove_blockers(w);
                    let note = if flags.is_empty() {
                        style("clean".to_string()).color256(theme::OK)
                    } else {
                        style(flags.join("; ")).color256(theme::WARN)
                    };
                    tui::emit_line(&format!(
                        "  {:<20} {}  {}",
                        w.name,
                        style(&w.branch).dim(),
                        note
                    ));
                    tui::emit_line(
                        &style(format!("      {}", w.path.display()))
                            .dim()
                            .to_string(),
                    );
                }
            }
            Err(e) => tui::emit_line(&format!("{} {e:#}", style("work:").red())),
        },
        "new" | "add" => {
            if rest.is_empty() {
                tui::emit_line(&style("usage: /work new <name>").dim().to_string());
                return;
            }
            match coop::work_new(rest) {
                Ok(wt) => {
                    tui::emit_line(
                        &style(format!(
                            "created worktree {} on {}",
                            wt.path.display(),
                            wt.branch
                        ))
                        .color256(theme::OK)
                        .to_string(),
                    );
                    tui::emit_line(
                        &style(format!(
                            "open a new terminal there: cd {}",
                            wt.path.display()
                        ))
                        .dim()
                        .to_string(),
                    );
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("work new:").red())),
            }
        }
        "remove" | "rm" => {
            let mut name = String::new();
            let mut force = false;
            for tok in rest.split_whitespace() {
                match tok {
                    "--force" | "-f" => force = true,
                    _ if name.is_empty() => name = tok.to_string(),
                    _ => {}
                }
            }
            if name.is_empty() {
                tui::emit_line(
                    &style("usage: /work remove <name> [--force]")
                        .dim()
                        .to_string(),
                );
                return;
            }
            match coop::work_remove(&name, force) {
                Ok(msg) => tui::emit_line(&style(msg).color256(theme::OK).to_string()),
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("work remove:").red())),
            }
        }
        other => tui::emit_line(
            &style(format!(
                "unknown /work subcommand '{other}' — list · new · remove"
            ))
            .dim()
            .to_string(),
        ),
    }
}

async fn handle_slash(
    input: &str,
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> SlashOutcome {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    // Resolve the typed spelling to a command IDENTITY first, then dispatch on that identity. Every
    // alias therefore reaches the same arm as its canonical name for free, and because the match
    // below is exhaustive over `SlashId`, a new row in `slash::BUILTINS` does not compile until it
    // has a handler here. The name lists used to live in five places; this is the only one left.
    // An empty name still means help: the retained REPL routes a bare `/` to the picker, but the
    // host bot and the plain REPL can both reach here with nothing after the slash.
    let Some(builtin) = slash::resolve(if name.is_empty() { "help" } else { name }) else {
        return slash_custom_or_unknown(name, arg);
    };
    match builtin.id {
        SlashId::Help => tui::emit_line(&style(slash::help_page()).dim().to_string()),
        SlashId::Quit => return SlashOutcome::Quit,
        SlashId::Clear => {
            rebuild_system(history, model_label);
            reset_per_session_state(); // fresh todos/cost/grants/@refs for the new conversation
            set_session_slug(None); // the next turn names + autosaves a brand-new session file
            update_live_history(history); // drop the old chat from the exit-flush snapshot too, so an
                                          // immediate window-close after /clear doesn't re-save it
            tui::emit_line(&style("(new conversation)").dim().to_string());
        }
        SlashId::Where => {
            tui::emit_line(&where_report());
            // In the REPL, "where" includes WHICH FILE this conversation is being written to.
            let sess = match current_session_slug() {
                Some(s) => format!("session:  {}", sessions_dir().join(format!("{s}.json")).display()),
                None => "session:  (not saved yet — named on the first autosave)".to_string(),
            };
            tui::emit_line(&style(sess).dim().to_string());
        }
        SlashId::Tokens => print_status_line(history, model_label),
        SlashId::Context => print_context(history, model_label),
        SlashId::Cost => print_cost(history, model_label),
        // /save + /load folded into /sessions (the current chat autosaves under its own name).
        SlashId::Save => {
            tui::emit_line(&style("→ use /sessions — restore / save / delete are all there now").dim().to_string());
        }
        SlashId::Sessions => {
            if let Err(e) = sessions_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("sessions:").red()));
            }
        }
        SlashId::Import => {
            if let Err(e) = import_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("import:").red()));
            }
        }
        // One keystroke back into the last conversation. `/sessions` could already restore, but it
        // costs a menu and knowing which of a dozen files is the newest — so in practice a reopened
        // terminal started from scratch even though the transcript was on disk the whole time.
        SlashId::Resume => {
            // Bare `/resume` carries the offer's origin label through, so opening another project's
            // conversation (only offered when this project has none) says so on the confirmation
            // line — including for pre-provenance files, where `load_session` has nothing to warn with.
            let (target, origin) = if arg.is_empty() {
                match most_recent_session() {
                    Some((slug, _, origin)) => (Some(slug), origin),
                    None => (None, None),
                }
            } else {
                (Some(sanitize_name(arg)), None)
            };
            match target {
                None => tui::emit_line(&style("no saved conversation to resume yet").dim().to_string()),
                Some(name) => match load_session(history, &name, model_label) {
                    Ok(n) => {
                        // A restore is a thread switch — the restored thread must not inherit the
                        // previous one's todos/cost/grants. Only on success: a failed load leaves
                        // the live thread (and its state) untouched.
                        reset_per_session_state();
                        // Replay so the restored thread is VISIBLE, not just present in the request:
                        // resuming into an empty-looking screen reads as "it didn't work".
                        agent::replay_transcript(history);
                        let origin_note = origin.map(|o| format!(" ({o})")).unwrap_or_default();
                        tui::emit_line(
                            &style(format!(
                                "⟲ resumed “{}”{origin_note} — {n} messages, context restored",
                                pretty_session_name(&name)
                            ))
                            .color256(splash::ACCENT)
                            .to_string(),
                        );
                    }
                    Err(e) => tui::emit_line(&format!("{} {e}", style("resume:").red())),
                },
            }
        }
        SlashId::Workflows => slash_workflows(arg).await,
        // Multi-window cooperation: who else is in this repo, what they changed, and committing one
        // window's work. `/work` manages the isolated-worktree mode.
        SlashId::Team => slash_team(arg).await,
        SlashId::Work => slash_work(arg),
        SlashId::Agents => slash_agents(arg),
        SlashId::Recover => {
            let repo_scope = crate::core::recovery::current_repo_scope();
            let offers = crate::core::recovery::scan_stale(&repo_scope);
            if offers.is_empty() {
                tui::emit_line(&style("no recoverable sessions found").dim().to_string());
            } else if arg == "discard" || arg == "drop" {
                for offer in &offers {
                    let _ = crate::core::recovery::discard(offer);
                }
                tui::emit_line(&style(format!("discarded {} recovery lease(s)", offers.len())).dim().to_string());
            } else {
                // Restore the newest offer. Side effects are never auto-replayed — only history + draft.
                let offer = &offers[0];
                match crate::core::recovery::accept(offer) {
                    Ok((restored, draft)) => {
                        *history = restored;
                        migrate_legacy_prompt_lanes(history, model_label);
                        refresh_prompt_lanes_for_thread_switch(history, model_label);
                        // Same thread-switch contract as /resume: the crashed thread's todos/cost/
                        // grants belong to it, not to whatever was live before accepting.
                        reset_per_session_state();
                        agent::replay_transcript(history);
                        if let Some(d) = draft {
                            tui::set_draft(&d);
                            tui::emit_line(&style("restored interrupted draft into the input box (not submitted)").dim().to_string());
                        }
                        if offer.manifest.side_effects_possible {
                            let checkpoint = offer
                                .manifest
                                .checkpoint_id
                                .map(|id| format!(" Check Time Machine checkpoint #{id} before retrying."))
                                .unwrap_or_else(|| " Check Time Machine before retrying.".to_string());
                            tui::emit_line(
                                &style(format!(
                                    "⚠ a previous tool may already have completed; retrying could repeat its side effect.{checkpoint}"
                                ))
                                .color256(theme::WARN)
                                .to_string(),
                            );
                        }
                    }
                    Err(e) => tui::emit_line(&format!("{} {e}", style("recover:").red())),
                }
            }
        }
        SlashId::Compact => {
            // Harvest what the older turns touched BEFORE they collapse — once summarized their tool
            // calls are gone, so the tree must read the history while it's still whole.
            let tp = agent::compact::context_touchpoints(history);
            tui::emit_line(&style("compacting… (Esc to stop)").dim().to_string());
            // Interruptible: the summarizer call is a network round-trip on the REPL's own thread.
            // Without this the whole app is frozen until it returns (or the 300s read timeout).
            match cancellable_slash(compact_now(history)).await {
                Some(Ok((b, a))) => print_compact_summary(b, a, &tp),
                Some(Err(e)) => tui::emit_line(&format!("{} {e}", style("compact:").red())),
                // Cancelled before the summary landed. `compact_history` only splices AFTER a
                // non-empty summary returns, so dropping the future leaves history untouched.
                None => tui::emit_line(&theme::muted("⏹ compact stopped — context unchanged.").to_string()),
            }
        }
        SlashId::Handoff => {
            if arg.trim().is_empty() {
                tui::emit_line(&style("usage: /handoff <new goal> — start a fresh thread carrying only what matters for it").dim().to_string());
            } else {
                tui::emit_line(&style("handing off…").dim().to_string());
                // Same cancellable wrapper as /compact: this is a blocking model call inside the
                // REPL loop, so without an armed token Esc can't reach it.
                match cancellable_slash(handoff_now(history, arg.trim())).await {
                    Some(Ok(summary)) => {
                        // Fresh thread: new system prompt, the goal-relevant extraction seeded as
                        // context, todos cleared, destructive-op session grants re-armed (like /clear).
                        rebuild_system(history, model_label);
                        // The marker prefix keeps the seed alive through lane rewrites (/config,
                        // /model, resume) — `leading_system_count` stops at it, so lane splices go
                        // around the seed instead of overwriting it.
                        history.push(Message::system(format!(
                            "{}\n{summary}",
                            agent::compact::HANDOFF_MARKER_PREFIX
                        )));
                        reset_per_session_state();
                        // The finished conversation keeps its file; the handoff starts a NEW one.
                        // Without re-slugging, the very next autosave overwrote the previous
                        // thread's saved transcript with this freshly seeded stub.
                        let previous = current_session_slug();
                        set_session_slug(None);
                        update_live_history(history);
                        tui::emit_line(&style("handoff — fresh thread seeded with the relevant context").color256(splash::ACCENT).to_string());
                        // Name the thread being left behind, so the full transcript is findable.
                        if let Some(prev) = previous {
                            tui::emit_line(
                                &style(format!("  (the previous thread stays saved as “{prev}” — /sessions to reopen it)"))
                                    .dim()
                                    .to_string(),
                            );
                        }
                        return SlashOutcome::Submit(arg.trim().to_string());
                    }
                    Some(Err(e)) => tui::emit_line(&format!("{} {e}", style("handoff:").red())),
                    // Cancelled before the extraction landed. Nothing was rebuilt, so the current
                    // thread continues untouched.
                    None => tui::emit_line(&theme::muted("⏹ handoff stopped — thread unchanged.").to_string()),
                }
            }
        }
        SlashId::Goal => {
            // Goal mode: run cap-free with smart retry until the model declares completion
            // (`goal_complete`) AND the verify gate passes. `/goal off` (or bare `/goal`) turns it off.
            let a = arg.trim();
            if a.is_empty() || a.eq_ignore_ascii_case("off") || a.eq_ignore_ascii_case("stop") {
                crate::agent::goal::set_goal(None);
                crate::agent::goal::arm(false);
                crate::agent::goal::clear();
                tui::emit_line(&style("goal mode off.").dim().to_string());
            } else {
                // Arm the tool gate + record the goal for every subsequent turn, and drain any stale
                // completion claim from a previous goal so it can't leak into this one.
                crate::agent::goal::set_goal(Some(a.to_string()));
                crate::agent::goal::arm(true);
                crate::agent::goal::clear();
                tui::emit_line(
                    &style("🎯 goal mode: running until done (self-declared + verified). Esc to cancel.")
                        .color256(splash::ACCENT)
                        .to_string(),
                );
                // Kick off immediately by submitting the goal text as the first user turn.
                return SlashOutcome::Submit(a.to_string());
            }
        }
        SlashId::Lsp => {
            use crate::agent::lsp::LSP;
            let sub = arg.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => tui::emit_line(&LSP.status().render()),
                "on" | "enable" => match LSP.enable() {
                    Ok(_) => tui::emit_line(
                        &style("LSP on — references · definition · symbols · symbol_replace/insert · diagnostics (rust/python/js-ts; servers start lazily on first use; rust-analyzer can use ~1–3GB RAM). /lsp off to stop.")
                            .color256(splash::ACCENT).to_string(),
                    ),
                    Err(e) => tui::emit_line(&format!("{} {e}", style("lsp:").red())),
                },
                "off" | "disable" => {
                    LSP.disable();
                    tui::emit_line(&style("LSP off — servers shut down, RAM reclaimed.").dim().to_string());
                }
                "restart" => {
                    LSP.disable();
                    match LSP.enable() {
                        Ok(_) => tui::emit_line(&style("LSP restarted.").dim().to_string()),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("lsp:").red())),
                    }
                }
                "edits" => {
                    let mode = arg.split_whitespace().nth(1).unwrap_or("").to_ascii_lowercase();
                    match mode.as_str() {
                        "on" => {
                            LSP.set_edit_feedback(true);
                            tui::emit_line(&style("LSP edit feedback on — new diagnostics fold into edit results.").dim().to_string());
                        }
                        "off" => {
                            LSP.set_edit_feedback(false);
                            tui::emit_line(&style("LSP edit feedback off.").dim().to_string());
                        }
                        _ => tui::emit_line(
                            &style(format!(
                                "usage: /lsp edits on|off  (currently {})",
                                if LSP.edit_feedback_enabled() { "on" } else { "off" }
                            ))
                            .dim()
                            .to_string(),
                        ),
                    }
                }
                other => tui::emit_line(
                    &style(format!("usage: /lsp [status|on|off|restart|edits on|off]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Reach => {
            use crate::agent::reach;
            let sub = arg.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => tui::emit_line(&reach::render_passive()),
                "doctor" | "dr" | "check" => {
                    tui::emit_line(&style("probing every backend (a few seconds)…").dim().to_string());
                    let reports = reach::doctor().await;
                    tui::emit_line(&reach::render_report(&reports));
                }
                other => tui::emit_line(
                    &style(format!("usage: /reach [status|doctor]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Update => {
            // Talks to GitHub, so wrap in `cancellable_slash` for Esc; owns stdin through the
            // dialoguer picker, which is why `tui::slash_takes_stdin` suspends the frame for it.
            match cancellable_slash(features::update::run()).await {
                None => tui::emit_line(&style("update cancelled").dim().to_string()),
                Some(Err(e)) => tui::emit_line(&format!("{} {e:#}", style("update:").red())),
                Some(Ok(())) => {}
            }
        }
        SlashId::Approval => {
            let requested = arg.split_whitespace().next().unwrap_or("status");
            let mut cfg = cli_config::load();
            if requested.is_empty() || matches!(requested, "status" | "st") {
                tui::emit_line(&style(format!("approval: {} · ask=prompt · smart=read-only auto · yolo=pre-authorized", approval_mode())).dim().to_string());
            } else if let Ok(mode) = requested.parse::<ApprovalMode>() {
                cfg.set_approval_mode(mode);
                match cli_config::save(&cfg) {
                    Ok(_) => tui::emit_line(&style(format!("approval → {mode}")).color256(splash::ACCENT).to_string()),
                    Err(e) => tui::emit_line(&format!("{} {e}", style("approval:").red())),
                }
            } else {
                tui::emit_line(&style("usage: /approval ask|smart|yolo").dim().to_string());
            }
        }
        SlashId::Yolo => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Yolo { ApprovalMode::Ask } else { ApprovalMode::Yolo };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /yolo alias)")).color256(splash::ACCENT).to_string());
        }
        SlashId::Smart => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Smart { ApprovalMode::Ask } else { ApprovalMode::Smart };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /smart alias)")).color256(splash::ACCENT).to_string());
        }
        SlashId::Ultimate => {
            let mut cfg = cli_config::load();
            let now = !cfg.ultimate.unwrap_or(false);
            cfg.ultimate = Some(now);
            if now {
                // ultracode = max effort + orchestrate-by-default: pin max, bypass auto-detect.
                cfg.reasoning_effort = Some("max".to_string());
                cfg.auto_effort = Some(false);
            } else {
                // back to the default: auto-detect ON, no pinned tier.
                cfg.reasoning_effort = None;
                cfg.auto_effort = None;
            }
            match cli_config::save(&cfg) {
                Ok(_) if now => tui::emit_line(
                    &style("✦ ultimate ON — max reasoning effort every turn + prefers launching workflows for fan-out-able tasks. /ultimate again to turn it off.")
                        .color256(splash::ACCENT).to_string(),
                ),
                Ok(_) => tui::emit_line(&style("ultimate OFF — effort back to auto-detect, no orchestration nudge.").dim().to_string()),
                Err(e) => tui::emit_line(&format!("{} {e}", style("ultimate:").red())),
            }
            // Recolour the input box to match: gold framing while ultimate is ON (mirrors the
            // `✦ ultimate` status chip), moonlight when OFF. Reflects the effective state — an
            // env-forced ON wins over the toggle, so read it back rather than trusting `now`.
            tui::set_ultimate(cli_config::ultimate_enabled());
            if std::env::var("AIZEN_ULTIMATE").is_ok() {
                tui::emit_line(&style("(note: AIZEN_ULTIMATE is set in your environment — it forces ultimate ON regardless of this toggle)").dim().to_string());
            }
        }
        SlashId::Effort => {
            let sub = arg.trim().to_ascii_lowercase();
            match sub.as_str() {
                // No arg → the interactive drag slider (falls back to a text report off-TTY).
                "" => effort_slider_flow(),
                // `status`/`st` → the plain text report (no slider).
                "status" | "st" => effort_status_report(),
                "auto" | "on" => {
                    let mut cfg = cli_config::load();
                    cfg.auto_effort = None; // None ⇒ ON (the default); clears any explicit off.
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort auto ON — each turn's effort is detected from your message (keyword + complexity).")
                                .color256(splash::ACCENT).to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "off" => {
                    let mut cfg = cli_config::load();
                    cfg.auto_effort = Some(false);
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort auto OFF — every turn uses the fixed reasoning_effort (or omits it if unset).")
                                .dim().to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "low" | "medium" | "high" | "xhigh" | "max" => {
                    let mut cfg = cli_config::load();
                    cfg.reasoning_effort = Some(sub.clone());
                    cfg.auto_effort = Some(false); // pinning a fixed tier turns auto off.
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style(format!("effort pinned to {sub} (auto off) — every turn now sends reasoning_effort={sub}."))
                                .color256(splash::ACCENT).to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "none" | "clear" => {
                    let mut cfg = cli_config::load();
                    cfg.reasoning_effort = None;
                    cfg.auto_effort = None; // back to the default (auto ON, no fixed tier).
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort cleared — auto ON, no fixed tier (requests omit reasoning_effort unless auto detects one).")
                                .dim().to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                other => tui::emit_line(
                    &style(format!("usage: /effort [auto|off|low|medium|high|xhigh|max|none]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Provider => {
            let selected = if arg.eq_ignore_ascii_case("add") || arg.eq_ignore_ascii_case("manage") {
                let mut cfg = cli_config::load();
                config_ui::config_edit_providers(&mut cfg).await.and_then(|_| {
                    cli_config::save(&cfg)?;
                    Ok(None)
                })
            } else if arg.is_empty() {
                provider_menu().await
            } else {
                activate_provider_profile(arg).map(Some)
            };
            match selected {
                Ok(Some(profile)) => {
                    *model_label = profile.model.clone();
                    cli_config::pin_session_model(&profile.model); // switching provider is a deliberate model change for THIS window
                    refresh_prompt_lanes_in_place(history, model_label);
                    tui::emit_line(
                        &style(format!(
                            "provider → {} · {} · {}",
                            profile.name,
                            profile.model,
                            config_ui::redact_url_userinfo(&profile.base_url)
                        ))
                        .color256(splash::ACCENT)
                        .to_string(),
                    );
                    let overridden: Vec<&str> = ["BASE_URL", "API_KEY", "MODEL"]
                        .into_iter()
                        .filter(|name| cli_config::branded_env(name).is_some())
                        .collect();
                    if !overridden.is_empty() {
                        tui::emit_line(
                            &style(format!(
                                "note: AIZEN_{} override the selected profile at runtime",
                                overridden.join(" / AIZEN_")
                            ))
                            .color256(theme::WARN)
                            .to_string(),
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => tui::note_line(&format!("{} {e}", style("provider:").red())),
            }
        }
        SlashId::Model => {
            if let Err(e) = slash_model(model_label).await {
                if tui::active() {
                    tui::emit_line(&format!("{} {e}", style("model:").red()));
                } else {
                    tui::note_line(&format!("{} {e}", style("model:").red()));
                }
            } else {
                // Also in place: the help text promises `/model` "switches models mid-session", and a
                // switch that silently discarded the session would make that promise a lie. The stable
                // lane carries `model:`, so it must be rewritten — see `refresh_prompt_lanes_in_place`.
                refresh_prompt_lanes_in_place(history, model_label);
            }
        }
        SlashId::Config => {
            if let Err(e) = config_ui::config_wizard().await {
                tui::note_line(&format!("{} {e}", style("config:").red()));
            }
            *model_label = cli_config::load().model.unwrap_or_else(|| model_label.clone());
            // The wizard just wrote config for THIS window on purpose, so adopt its model as the new
            // session pin rather than letting the startup pin override what the user just chose.
            cli_config::pin_session_model(model_label);
            // Refresh IN PLACE — retuning settings mid-chat must not end the conversation.
            refresh_prompt_lanes_in_place(history, model_label);
        }
        SlashId::Memory => {
            if let Err(e) = slash_memory(arg) {
                tui::note_line(&format!("{} {e}", style("memory:").red()));
            }
        }
        SlashId::Persona => {
            if let Err(e) = personas_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("persona:").red()));
            }
        }
        SlashId::Skills => {
            if let Err(e) = skills_menu().await {
                tui::note_line(&format!("{} {e}", style("skills:").red()));
            }
        }
        SlashId::Apps => {
            if let Err(e) = apps_menu().await {
                tui::note_line(&format!("{} {e}", style("apps:").red()));
            }
        }
        SlashId::Mcp => tui::emit_line(&crate::agent::mcp::summary()),
        SlashId::Browser => {
            #[cfg(feature = "browser")]
            {
                if matches!(arg, "doctor" | "check" | "probe") {
                    tui::emit_line(&style("probing browser profiles…").dim().to_string());
                    tui::emit_line(&crate::agent::browser::doctor().await);
                } else {
                    tui::emit_line(&crate::agent::browser::status());
                }
            }
            #[cfg(not(feature = "browser"))]
            tui::emit_line(&style("browser tools are not included in this build (build with --features browser)").dim().to_string());
        }
        SlashId::Tools => slash_tools(arg).await,
        SlashId::Commands => match commands::summary() {
            Some(s) => tui::emit_line(&style(s).dim().to_string()),
            None => tui::emit_line(
                &style("No custom commands yet. Drop a markdown file in ~/.aizen/commands/ (or ./.aizen/commands/ for this project) — see /help.").dim().to_string()
            ),
        },
        SlashId::Telegram => {
            if let Err(e) = telegram_menu().await {
                tui::note_line(&format!("{} {e}", style("telegram:").red()));
            }
        }
        // `/serve` kept as a direct shortcut to the daemon (also reachable via the Telegram menu).
        SlashId::Serve => {
            if let Err(e) = hostbot::run_serve(Vec::new()).await {
                tui::note_line(&format!("{} {e}", style("serve:").red()));
            }
        }
        // ── time machine (git snapshots) ──
        // `/timemachine` is ONE command: it opens the checkpoint list, and picking a row rewinds to
        // that code + chat. No `pick`/`restore` argument to remember, no separate read-only print —
        // the list itself shows the history, so browsing and restoring are the same gesture (Esc
        // leaves without touching anything).
        SlashId::Timemachine => {
            if let Err(e) = timemachine_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("time:").red()));
            }
        }
        // Capture the conversation alongside the tree so a pick in `/timemachine` can rewind chat as
        // well as code — a `/checkpoint` is a deliberate save point where the chat is worth keeping,
        // unlike the loop's per-edit auto-snapshots (which restore files only).
        SlashId::Checkpoint => match timemachine::save_with_chat(arg, false, history) {
            Ok(s) => tui::emit_line(&format!(
                "{} #{} saved ({})",
                style("✓ checkpoint").color256(splash::ACCENT),
                s.id,
                if s.has_chat { "code + chat" } else { "files only" }
            )),
            Err(e) => tui::emit_line(&style(format!("checkpoint: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // `/diff` — see what changed before deciding to rewind. Argument forms mirror the CLI:
        // bare = active checkpoint vs disk, `#5` = that checkpoint vs disk, `#1 #2` = the pair.
        // `-p`/`--patch` anywhere switches from stat to hunks; anything after `--` narrows to paths.
        SlashId::Diff => {
            let mut sides: Vec<String> = Vec::new();
            let mut paths: Vec<String> = Vec::new();
            let mut patch = false;
            let mut after_sep = false;
            for tok in arg.split_whitespace() {
                match tok {
                    "--" => after_sep = true,
                    "-p" | "--patch" => patch = true,
                    _ if after_sep => paths.push(tok.to_string()),
                    _ => sides.push(tok.to_string()),
                }
            }
            let (from, to) = (sides.first().cloned(), sides.get(1).cloned());
            match build_time_diff(from, to, paths, patch) {
                // Must go through `emit_line`: raw `println!` from inside the REPL is wiped by the
                // retained render thread's next repaint.
                Ok(report) => {
                    for line in diff_lines(&report, "-- <path>") {
                        tui::emit_line(&line);
                    }
                }
                Err(e) => tui::emit_line(&style(format!("diff: {e}")).color256(crate::ui::theme::WARN).to_string()),
            }
        }
        SlashId::Undo => match timemachine::undo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏪ rewound to").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("undo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        SlashId::Redo => match timemachine::redo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏩ re-applied").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("redo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // Codebase index: scan the repo into a semantic chunk index for `codebase_search` +
        // per-turn retrieval injection. `/init` incrementally refreshes; `/init --force` rebuilds
        // from scratch; `/init --status` shows the current index without scanning. Esc cancels.
        SlashId::Init => {
            slash_init(arg).await;
        }
    }
    SlashOutcome::Continue
}
/// Not a built-in: either a user-defined command (`~/.aizen/commands/<name>.md`) whose template we
/// expand and run as a normal chat turn, or nothing we know at all.
///
/// Split out of the dispatch match so that match can be exhaustive over [`SlashId`] — a catch-all
/// arm would have silently absorbed any command whose handler someone forgot to write.
fn slash_custom_or_unknown(name: &str, arg: &str) -> SlashOutcome {
    match commands::find(name) {
        Some(cmd) => match commands::expand(&cmd, arg) {
            Ok(prompt) if !prompt.trim().is_empty() => return SlashOutcome::Submit(prompt),
            Ok(_) => tui::emit_line(
                &style(format!("/{name} expanded to an empty prompt"))
                    .dim()
                    .to_string(),
            ),
            Err(e) => tui::emit_line(&format!("{} {e}", style(format!("/{name}:")).red())),
        },
        None => tui::emit_line(
            &style(format!("unknown command /{name} — try /help"))
                .dim()
                .to_string(),
        ),
    }
    SlashOutcome::Continue
}

/// `/sessions` — the conversation manager (replaces the old `/save` + `/load`): pick a saved
/// conversation to RESTORE, save the current one under a name, or delete one. The live chat
/// autosaves into its OWN named file after every turn, so there's always something to come back to.
async fn sessions_menu(history: &mut Vec<Message>, model_label: &str) -> Result<()> {
    loop {
        let theme = ui_theme();
        // Newest first, with provenance: age, origin project for foreign/unlabeled files, a
        // "● current" marker on the live conversation's file, and corrupt files called out as
        // unreadable instead of posing as plausible "0 msgs" sessions.
        let pool = scan_sessions();
        let names: Vec<String> = pool.iter().map(|s| s.name.clone()).collect();
        let n_sessions = pool.len();
        let current = current_session_slug();
        let mut items: Vec<String> = pool
            .iter()
            .map(|s| {
                let count = match s.msgs {
                    Some(n) => format!("{n} msg{}", if n == 1 { "" } else { "s" }),
                    None => "(unreadable)".to_string(),
                };
                let mut row = format!(
                    "{} {}  —  {count} · {}",
                    icons::g(icons::slash("sessions")),
                    s.name,
                    fmt_session_age(s.mtime_ms)
                );
                if s.here == Some(false) {
                    row.push_str(&format!(" · {}", session_origin_label(s.meta.as_ref())));
                }
                if current.as_deref() == Some(s.name.as_str()) {
                    row.push_str(" · ● current");
                }
                row
            })
            .collect();
        items.push("+ Save current conversation…".to_string());
        if n_sessions > 0 {
            items.push("Delete a session".to_string());
        }
        items.push("Back".to_string());

        let prompt = if n_sessions == 0 {
            "Sessions — none saved yet (Esc to go back)".to_string()
        } else {
            format!("Sessions — {n_sessions} saved, newest first · pick one to restore (Esc to go back)")
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

        if pick < n_sessions {
            let name = &names[pick];
            match load_session(history, name, model_label) {
                Ok(n) => {
                    reset_per_session_state(); // thread switch — same contract as /resume
                    println!(
                        "{}",
                        style(format!(
                            "restored '{}' ({n} messages)",
                            pretty_session_name(name)
                        ))
                        .color256(splash::ACCENT)
                    );
                    agent::replay_transcript(history);
                    return Ok(());
                }
                Err(e) => tui::note_line(&format!("{} {e}", style("restore:").red())),
            }
        } else if items[pick].starts_with("+ Save") {
            let suggested = suggest_session_name(history);
            let name: String = Input::with_theme(&theme)
                .with_prompt("Save as")
                .with_initial_text(suggested)
                .interact_text()
                .unwrap_or_default();
            if !name.trim().is_empty() {
                // Saving over a DIFFERENT conversation's file is destructive — confirm it. Re-saving
                // the live conversation's own file is the normal case and stays silent.
                let target = sanitize_name(name.trim());
                if let Some(why) = session_save_name_error(&name) {
                    eprintln!("{} {why}", style("save:").red());
                    continue;
                }
                let exists = sessions_dir().join(format!("{target}.json")).exists();
                if exists && current.as_deref() != Some(target.as_str()) {
                    let overwrite = Confirm::with_theme(&theme)
                        .with_prompt(format!(
                            "'{}' already exists — overwrite it?",
                            pretty_session_name(&target)
                        ))
                        .default(false)
                        .interact_opt()?
                        .unwrap_or(false);
                    if !overwrite {
                        continue;
                    }
                }
                match save_session(history, name.trim(), Some(model_label)) {
                    Ok(_) => {
                        // Pin the session to this name so later autosaves keep rewriting the SAME file
                        // (both paths route through `sanitize_name`, so the raw name maps to one file).
                        set_session_slug(Some(name.trim().to_string()));
                        update_live_history(history); // exit-flush snapshot follows the pinned name
                        println!(
                            "{}",
                            style(format!("saved '{}'", name.trim())).color256(splash::ACCENT)
                        );
                    }
                    Err(e) => tui::note_line(&format!("{} {e}", style("save:").red())),
                }
            }
        } else if items[pick] == "Delete a session" {
            if let Ok(Some(i)) = Select::with_theme(&theme)
                .with_prompt("Delete which session? (Esc to cancel)")
                .items(&names)
                .default(0)
                .interact_opt()
            {
                let slug = &names[i];
                let pretty = pretty_session_name(slug);
                let confirmed = Confirm::with_theme(&theme)
                    .with_prompt(format!("Delete '{pretty}' permanently?"))
                    .default(false)
                    .interact_opt()?
                    .unwrap_or(false);
                if !confirmed {
                    continue;
                }
                match delete_session(slug) {
                    Ok(_) => {
                        if current_session_slug().as_deref() == Some(slug.as_str()) {
                            set_session_slug(None);
                        }
                        println!(
                            "{}",
                            style(format!("deleted '{pretty}'")).color256(splash::ACCENT)
                        );
                    }
                    Err(e) => tui::note_line(&format!("{} {e}", style("delete:").red())),
                }
            }
        } else {
            return Ok(()); // Back
        }
    }
}

/// `/import` — pick a conversation recorded by Claude Code or Codex (for THIS project) and resume
/// it inside aizen. The foreign transcript is parsed into `Vec<Message>`, repaired so it satisfies
/// `assert_valid_history`, then handed to the SAME thread-switch path `/resume` uses: refresh the
/// prompt lanes for the current project + model, reset per-session state, and replay so the
/// restored thread is VISIBLE rather than silently present.
///
/// Foreign transcripts are never autosaved back over themselves — the imported conversation becomes
/// the live one and is autosaved under a fresh aizen slug from then on, exactly like a `/resume`
/// of an aizen session. The source file is only ever READ.
async fn import_menu(history: &mut Vec<Message>, model_label: &str) -> Result<()> {
    let theme = ui_theme();
    let pool = features::foreign_session::discover(&config::project_root());
    if pool.is_empty() {
        tui::emit_line(
            &style("no Claude Code or Codex transcripts found for this project")
                .dim()
                .to_string(),
        );
        tui::emit_line(
            &style("(they appear here once you've used `claude` or `codex` in this directory)")
                .dim()
                .to_string(),
        );
        return Ok(());
    }
    // Clip to the terminal, minus dialoguer's own `❯ ` prefix and one cell of right margin. An item
    // that overflows gets WRAPPED onto a second line, which breaks the column alignment for every row
    // below it — the whole reason the list is scannable.
    let width = console::Term::stdout().size().1 as usize;
    let items: Vec<String> = pool
        .iter()
        .map(|s| s.row(fmt_session_age_compact, width.saturating_sub(4).max(24)))
        .collect();
    let prompt = format!("Import — {} conversations from claude/codex", pool.len());
    let pick = match Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    let session = &pool[pick];
    match features::foreign_session::load(session) {
        Ok(imported) => {
            // Drop any system lanes the source CLI's harness left in (already filtered in parse,
            // but defend against a future schema that embeds them) before splicing aizen's own.
            *history = imported;
            refresh_prompt_lanes_for_thread_switch(history, model_label);
            // Same thread-switch contract as /resume: the foreign thread's todos/cost/grants
            // belong to it, not to whatever was live before the import.
            reset_per_session_state();
            set_session_slug(None); // the imported chat autosaves under a fresh aizen slug from here
            agent::replay_transcript(history);
            let tag = match session.cli {
                features::foreign_session::Cli::Claude => "claude",
                features::foreign_session::Cli::Codex => "codex",
            };
            tui::emit_line(
                &style(format!(
                    "⇲ imported “{}” from {} — {} messages, context restored",
                    if session.first_prompt.is_empty() {
                        "(no prompt)"
                    } else {
                        &session.first_prompt
                    },
                    tag,
                    history.len()
                ))
                .color256(splash::ACCENT)
                .to_string(),
            );
            tui::emit_line(
                &style(format!("  source: {}", session.path.display()))
                    .dim()
                    .to_string(),
            );
        }
        Err(e) => tui::emit_line(&format!("{} {e}", style("import:").red())),
    }
    Ok(())
}

/// `aizen import [path]` — CLI surface. No path lists every foreign transcript for this project;
/// a path loads that file's transcript and prints a one-line summary (the CLI can't resume into a
/// REPL, so it reports what WOULD be loaded — the actual resume happens via `/import` in the REPL).
async fn run_import(path: Option<String>) -> Result<()> {
    match path {
        Some(p) => {
            let p = std::path::PathBuf::from(&p);
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            // Detect the CLI from the file's own shape rather than the path: a Claude line has a
            // top-level `type` that is "user"/"assistant"/"mode"/…; a Codex line has `session_meta`
            // or `response_item`. Sniff the first parseable line.
            let cli = sniff_cli(&bytes);
            let sess = features::foreign_session::ForeignSession {
                cli,
                path: p.clone(),
                cwd: String::new(),
                mtime_ms: None,
                turns: 0,
                first_prompt: String::new(),
            };
            match features::foreign_session::load(&sess) {
                Ok(msgs) => {
                    let tag = match cli {
                        features::foreign_session::Cli::Claude => "claude",
                        features::foreign_session::Cli::Codex => "codex",
                    };
                    println!(
                        "{}",
                        style(format!(
                            "⇲ {} transcript — {} messages ready to resume",
                            tag,
                            msgs.len()
                        ))
                        .color256(splash::ACCENT)
                    );
                    println!("  source: {}", p.display());
                    println!(
                        "  {}",
                        style("open the REPL in this project and run /import to resume it").dim()
                    );
                    Ok(())
                }
                Err(e) => anyhow::bail!("import: {e}"),
            }
        }
        None => {
            let pool = features::foreign_session::discover(&config::project_root());
            if pool.is_empty() {
                println!(
                    "{}",
                    style("no Claude Code or Codex transcripts found for this project").dim()
                );
                println!(
                    "{}",
                    style(
                        "(they appear here once you've used `claude` or `codex` in this directory)"
                    )
                    .dim()
                );
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "Foreign transcripts for this project ({}), newest first:",
                    pool.len()
                ))
                .color256(splash::ACCENT)
            );
            for s in &pool {
                let tag = match s.cli {
                    features::foreign_session::Cli::Claude => "claude",
                    features::foreign_session::Cli::Codex => "codex",
                };
                println!(
                    "  [{}] {:>6} · {} turns · {}",
                    tag,
                    fmt_session_age(s.mtime_ms),
                    s.turns,
                    s.path.display()
                );
            }
            println!(
                "{}",
                style("resume one with: /import  (in the REPL)  or  aizen import <path>").dim()
            );
            Ok(())
        }
    }
}

/// Decide which CLI a transcript belongs to from its content, not its path. Falls back to Claude
/// (the more permissive parser — it ignores unrecognized line types) when the sniff is inconclusive.
fn sniff_cli(bytes: &[u8]) -> features::foreign_session::Cli {
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "session_meta"
            || ty == "response_item"
            || ty == "event_msg"
            || ty == "turn_context"
        {
            return features::foreign_session::Cli::Codex;
        }
        if ty == "user" || ty == "assistant" || ty == "mode" || ty == "file-history-snapshot" {
            return features::foreign_session::Cli::Claude;
        }
    }
    features::foreign_session::Cli::Claude
}

/// Switch to one saved provider profile. Root endpoint fields are updated atomically, so the next
/// turn, aside question, and health probe all see the same provider without restarting the REPL.
fn activate_provider_profile(name: &str) -> Result<cli_config::ProviderProfile> {
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

async fn provider_menu() -> Result<Option<cli_config::ProviderProfile>> {
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
async fn slash_model(model_label: &mut String) -> Result<()> {
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

/// Resolve base URL + API key + model: explicit flag/env (clap) > saved config. Errors name all
/// three ways to provide a missing value.
fn resolve_endpoint(
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<(String, String, String)> {
    let cfg = cli_config::load();
    // Precedence: explicit `--flag` (already folded into the args) > env (`AIZEN_*`) > saved config.
    // Reading env here (not just via clap) means the bare REPL honors it too.
    let base_url = base_url
        .or_else(|| cli_config::branded_env("BASE_URL"))
        .or(cfg.base_url)
        .context("no base URL — run `aizen config` (interactive setup), or pass --base-url / set AIZEN_BASE_URL")?;
    let api_key = api_key
        .or_else(|| cli_config::branded_env("API_KEY"))
        .or(cfg.api_key)
        .context("no API key — run `aizen config` (interactive setup), or pass --api-key / set AIZEN_API_KEY")?;
    // Session pin sits between env and disk: a REPL window stays on the model IT resolved, so a
    // sibling window running `/model` (which rewrites the shared cli-config.json) can't switch this
    // one out from under it on the next turn. Non-REPL callers never pin ⇒ they read `cfg.model`.
    let model = model
        .or_else(|| cli_config::branded_env("MODEL"))
        .or_else(cli_config::session_model)
        .or(cfg.model)
        .context("no model — run `aizen config` (interactive setup) or `aizen models` to list, or pass --model / set AIZEN_MODEL")?;
    Ok((base_url, api_key, model))
}

fn resolve_base_key(base_url: Option<String>, api_key: Option<String>) -> Result<(String, String)> {
    let cfg = cli_config::load();
    let base_url = base_url
        .or_else(|| cli_config::branded_env("BASE_URL"))
        .or(cfg.base_url)
        .context("no base URL — run `aizen config`")?;
    let api_key = api_key
        .or_else(|| cli_config::branded_env("API_KEY"))
        .or(cfg.api_key)
        .context("no API key — run `aizen config`")?;
    Ok((base_url, api_key))
}

/// Why this client carries NO total-request `timeout`.
///
/// 0.5.2 added `.timeout(1800s)` here as "a backstop under any path nobody has enumerated". That was
/// a bug, and the reason is worth keeping: reqwest's total timeout is applied "from when the request
/// starts connecting until the response body has finished" — a whole-response deadline, not a
/// header-phase one. This very client is what the REPL hands to `stream_chat_with_tools_eager` for
/// every turn, so the ceiling did not merely cap pathological hangs: it cut off a HEALTHY stream that
/// was still emitting tokens, 30 minutes in, losing the entire turn. A deep reasoning run with many
/// tool calls reaches that legitimately.
///
/// The stall protection that a streaming path actually needs is shaped per-event, not per-response,
/// and already exists in two layers: `read_timeout` below (the socket going byte-silent) and
/// `llm::client`'s inter-event watchdog, which re-arms on every SSE event and so distinguishes "the
/// gateway stopped writing" from "the answer is long". A total deadline cannot make that distinction,
/// which is exactly why it is wrong here.
///
/// One-shot clients (health probe, update check, model discovery) DO set a total timeout — nothing
/// they fetch streams, so "the whole response took too long" is a meaningful failure there.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

/// Short-timeout client for the health probe only — a dead endpoint must fail the chip fast, not
/// wait out the chat client's 300s read timeout. Connect + total request each capped at 4s.
fn health_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION"), " health"))
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .context("building health HTTP client")
}

/// How often the idle `●` chip re-probes the provider. Confirmed: 60s.
const HEALTH_POLL_SECS: u64 = 60;
/// A successful `GET /models` slower than this is painted yellow (unstable). Confirmed: 2s.
const HEALTH_SLOW_MS: u128 = 2_000;

/// Classify a single probe outcome into the idle-chip colour. Pure so it can be unit-tested
/// without a network. Rules (user-confirmed):
/// - Ok + latency ≤ 2s → green (`Ok`)
/// - Ok + latency > 2s → yellow (`Unstable`)
/// - Err classified Transient (429/5xx/timeout/transport) → yellow (`Unstable`)
/// - Err classified Permanent (400/401/403/404) → red (`Down`)
/// - Missing config (no base/key) is treated as Permanent → red
fn classify_health_probe(result: Result<std::time::Duration, anyhow::Error>) -> tui::HealthKind {
    match result {
        Ok(latency) if latency.as_millis() > HEALTH_SLOW_MS => tui::HealthKind::Unstable,
        Ok(_) => tui::HealthKind::Ok,
        Err(e) => match client::classify_api_error(&e) {
            client::ApiErrorKind::Permanent => tui::HealthKind::Down,
            client::ApiErrorKind::Transient => tui::HealthKind::Unstable,
        },
    }
}

/// Spawn the once-per-session batch reconciliation (M2b), off the hot path.
///
/// Three properties make an automatic pass that RETIRES facts acceptable here:
///
/// - **It fires rarely.** `should_run` gates on ≥8 waiting pairs or ≥7 days since the last pass, so a
///   store with nothing to resolve never pays a call.
/// - **It cannot run twice.** `batch_pass` takes the judge as `FnOnce`, and this task is spawned once
///   per REPL start, so "≤1 model call per session" is structural rather than remembered.
/// - **Everything it does is reversible.** Retirement is `supersedes:` + `revive`, never a delete, and
///   the summary line names what changed so the user can see it happened at all — a silent pass that
///   rewrites memory is the thing this design refuses.
///
/// Fully best-effort: any failure leaves the store exactly as it was and says nothing.
fn spawn_reconcile_pass() {
    tokio::spawn(async move {
        if !memory_auto_learn_enabled() {
            return; // the same switch that governs learning governs correcting
        }
        let Ok((pairs, live)) = memory::reconcile_inputs() else {
            return;
        };
        let today = memory::bloat::decay::today();
        if !memory::learning::reconcile::should_run(
            pairs.len(),
            memory::learning::reconcile::last_run().as_deref(),
            &today,
        ) {
            return;
        }
        let Ok((base, key, model)) = resolve_endpoint(None, None, None) else {
            return;
        };
        let Ok(http) = http_client() else { return };
        let ep = summarizer_endpoint(&base, &key, &model);
        let judge = |sys: &str, user: &str| -> Option<String> {
            let msgs = [
                Message::system(sys.to_string()),
                Message::user(user.to_string()),
            ];
            let fut = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(fut)
                    .ok()?
                    .content
            })
        };
        let report = memory::learning::reconcile::batch_pass(
            &pairs,
            judge,
            false, // this path APPLIES; the CLI is the dry-run surface
            &memory::learning::default_session_id(),
            &live,
        );
        // One line, and only when something actually changed. A background pass that narrates itself
        // every session is noise; one that changes memory in silence is worse.
        let acted = report
            .applied
            .iter()
            .filter(|a| !matches!(a.action, memory::learning::reconcile::Action::Review { .. }))
            .count();
        // Retirements are counted separately: "reconciled 3 facts" reads like bookkeeping, but a row
        // leaving the active view is the change a user would want to know about, and it is the half
        // that needs the undo hint. Only removals that reported no failure are counted.
        let dropped = report
            .applied
            .iter()
            .filter(|a| {
                matches!(
                    &a.action,
                    memory::learning::reconcile::Action::Confirm {
                        redundant: Some(_),
                        ..
                    }
                ) && !a.note.contains("kept")
            })
            .count();
        if acted > 0 {
            let what = if dropped > 0 {
                format!("reconciled {acted} memory fact(s), retiring {dropped} duplicate(s)")
            } else {
                format!("reconciled {acted} memory fact(s)")
            };
            tui::emit_line(
                &style(format!(
                    "⚖ {what} — `aizen memory list --superseded` to review, `revive <id>` to undo"
                ))
                .dim()
                .to_string(),
            );
        }
    });
}

/// Probe the newly selected provider immediately instead of waiting for the next 60-second poll tick.
fn spawn_health_probe_once() {
    tokio::spawn(async move {
        let kind = match (health_http_client(), resolve_base_key(None, None)) {
            (Ok(http), Ok((base, key))) => {
                let t0 = std::time::Instant::now();
                classify_health_probe(
                    client::probe_models(&http, &base, &key)
                        .await
                        .map(|_| t0.elapsed()),
                )
            }
            _ => tui::HealthKind::Down,
        };
        tui::set_health(kind);
    });
}

/// Spawn a background task that paints the idle `● ready` chip from a real `GET /models` probe.
/// Runs once immediately, then every [`HEALTH_POLL_SECS`]. Lives for the process (the REPL owns
/// the runtime); each tick re-resolves base_url/api_key so a mid-session `/config` takes effect
/// without a restart. Failures never surface as text — only as the chip colour.
fn spawn_health_poller() {
    tokio::spawn(async move {
        let http = match health_http_client() {
            Ok(c) => c,
            Err(_) => {
                tui::set_health(tui::HealthKind::Down);
                return;
            }
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEALTH_POLL_SECS));
        // The first tick completes immediately (tokio interval behaviour) → first probe is eager.
        loop {
            interval.tick().await;
            let kind = match resolve_base_key(None, None) {
                Ok((base, key)) => {
                    let t0 = std::time::Instant::now();
                    let result = client::probe_models(&http, &base, &key)
                        .await
                        .map(|_| t0.elapsed());
                    classify_health_probe(result)
                }
                // Not configured yet → permanent unavailability until /config. Don't lean on
                // classify_api_error (which would paint yellow for a message without an HTTP code).
                Err(_) => tui::HealthKind::Down,
            };
            tui::set_health(kind);
        }
    });
}

/// Spawn the long-lived off-to-the-side Q&A worker. It owns an unbounded channel (armed into
/// `core::aside`) and answers `?`-prefixed questions one at a time, WITHOUT touching the turn in
/// flight: it clones the read-only live-conversation snapshot, makes ONE tool-less model call, and
/// prints the answer through `tui::emit_line` (which the retained renderer serializes with the main
/// stream on its single render thread, so a mid-turn aside can never corrupt the frame). It never
/// mutates `history`, never arms cancel, never flips `WORKING` — the running turn is oblivious.
///
/// Errors are shown inline and swallowed: a failed side question must never take down the worker
/// (which would silently disable the feature for the rest of the session) nor the REPL.
fn spawn_aside_worker(http: reqwest::Client) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    crate::core::aside::arm(tx);
    tokio::spawn(async move {
        while let Some(question) = rx.recv().await {
            // Resolve the endpoint fresh per question: the user may have switched models with
            // `/model` since the worker was spawned, and an aside should follow that choice.
            let (base_url, api_key, model) = match resolve_endpoint(None, None, None) {
                Ok(t) => t,
                Err(_) => {
                    tui::emit_line(
                        &style("  ⁇ side question skipped — no model configured (/config).")
                            .dim()
                            .to_string(),
                    );
                    continue;
                }
            };
            // Read-only snapshot of the live conversation (kept current DURING the turn via
            // `on_progress`); cloned so we never hold the lock across the await.
            let snapshot = live_history_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let msgs = crate::core::aside::build_messages(&snapshot, &question);
            // Echo the question so the answer has a visible anchor in the transcript (dim, with a
            // `⁇` glyph, so it reads as an out-of-band aside distinct from a `❯` user turn).
            tui::emit_line(
                &style(format!("  ⁇ {question}"))
                    .color256(theme::MUTED)
                    .to_string(),
            );
            // ONE tool-less, non-streaming call. Empty tool slice ⇒ no tools offered.
            //
            // Wrapped in the SAME per-call deadline a sub-agent gets (`subagent_call_timeout`,
            // default 300s, `AIZEN_SUBAGENT_CALL_SECS`): the shared client carries no total-request
            // ceiling (removing that is what unblocks a legitimately long streamed turn — see
            // `http_client`), and `chat_with_tools` reads its body with `.json().await`, outside any
            // deadline. `read_timeout` resets on every byte, so a gateway that keepalive-drips
            // without ever finishing the body would park this worker forever, silently killing the
            // aside feature for the rest of the session. This is not a streamed answer, so a flat
            // per-call cap is exactly right — no inter-event watchdog applies here.
            let call = client::chat_with_tools(&http, &base_url, &api_key, &model, &msgs, &[]);
            let deadline = crate::agent::task_tool::subagent_call_timeout();
            let outcome = match tokio::time::timeout(deadline, call).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "side question timed out after {}s with no response",
                    deadline.as_secs()
                )),
            };
            match outcome {
                Ok(turn) => {
                    let answer = turn.content.unwrap_or_default();
                    if answer.trim().is_empty() {
                        tui::emit_line(&style("  ⁇ (no answer)").dim().to_string());
                    } else {
                        let shown = crate::ui::markdown::render_plain_blocks(answer.trim());
                        // Prefix every line dimly so the whole aside block reads as a margin note
                        // beside the main work, not as the model's task output.
                        for line in shown.lines() {
                            tui::emit_line(
                                &style(format!("  {line}"))
                                    .color256(theme::MUTED)
                                    .to_string(),
                            );
                        }
                    }
                }
                Err(e) => {
                    tui::emit_line(
                        &style(format!("  ⁇ side question failed: {e}"))
                            .color256(theme::WARN)
                            .to_string(),
                    );
                }
            }
        }
    });
}

async fn run_models(args: ModelsArgs) -> Result<()> {
    let (base_url, api_key) = resolve_base_key(args.base_url, args.api_key)?;
    // Codex has no stable OpenAI-style /models; print the curated experimental catalog.
    if crate::llm::oauth_codex::is_codex_base_url(&base_url) {
        let current = cli_config::load().model;
        println!("ChatGPT Codex models (experimental catalog):");
        for (id, label) in crate::llm::codex_models::CODEX_MODELS {
            let mark = if current.as_deref() == Some(*id) {
                " (default)"
            } else {
                ""
            };
            println!("{id}  · {label}  · codex{mark}");
        }
        if !crate::llm::oauth_codex::has_token() {
            println!("(not logged in — run: aizen auth login codex)");
        }
        return Ok(());
    }
    let http = http_client()?;
    let infos = client::fetch_models_info(&http, &base_url, &api_key)
        .await
        .context("fetching models")?;
    if infos.is_empty() {
        println!("(provider returned no models)");
        return Ok(());
    }
    let current = cli_config::load().model;
    let any_ctx = infos.iter().any(|m| m.context_length.is_some());
    for m in &infos {
        let mark = if current.as_deref() == Some(m.id.as_str()) {
            " (default)"
        } else {
            ""
        };
        let free = if m.is_free || client::is_free_model_id(&m.id) {
            "  · free"
        } else {
            ""
        };
        let ctx = match m.context_length {
            Some(n) if n >= 1000 => format!("  · ctx {}K", n / 1000),
            Some(n) => format!("  · ctx {n}"),
            None => String::new(),
        };
        println!("{}{free}{ctx}{mark}", m.id);
    }
    if !any_ctx {
        println!(
            "\n{}",
            style(
                "(this provider doesn't report context windows — the HUD estimates by model name)"
            )
            .dim()
        );
    }
    println!("\nset a default: `aizen config set --model <id>`");
    Ok(())
}

async fn run_chat(args: ChatArgs) -> Result<()> {
    let prompt = match args.prompt {
        Some(p) => p,
        None => read_stdin("reading prompt from stdin")?,
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt (pass --prompt or pipe text on stdin)");
    }
    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;

    let messages = vec![Message::user(prompt)];
    client::stream_chat_with_visual_contract(&http, &base_url, &api_key, &model, messages, true)
        .await
        .context("chat completion failed")?;
    Ok(())
}

async fn run_agent_cmd(args: AgentArgs) -> Result<()> {
    if args.task.trim().is_empty() {
        anyhow::bail!("empty task (pass the task as the first argument)");
    }
    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;

    // Session start: rebuild the always-on core for THIS project slug (STYLE + global prefs
    // only). Do not reuse a stale foreign-repo core.active — refresh_frozen_core is slug-aware.
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        &model,
        Some(&frozen),
    );

    // Registry includes the `task` sub-agent tool (depth 0); a spawned sub-agent uses a
    // role-scoped registry WITHOUT `task` (no recursion).
    let cli_approval = if args.yes {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Ask
    };
    arm_lsp_session();
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        cli_approval,
        resolve_ctx_window(&model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let max = args.max_iters.unwrap_or(25).max(1);
    let cfg = AgentConfig {
        max_iters: max,
        auto_extend_to: max.saturating_mul(2),
        approval_mode: cli_approval,
        context_window: resolve_ctx_window(&model).0,
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        ..Default::default()
    };

    // The model call, injected into the loop. http_ref/base/key/model are all Copy
    // (&Client / &str), so the closure stays `Fn` across the loop's repeated calls.
    let http_ref = &http;
    let base = base_url.as_str();
    let key = api_key.as_str();
    let model_ref = model.as_str();
    let registry_ref = &registry;
    let cfg_ref = &cfg;
    let eager_on = eager_enabled();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        if eager_on {
            let starter = agent::eager_starter(registry_ref, cfg_ref);
            client::stream_chat_with_tools_eager(
                http_ref,
                base,
                key,
                model_ref,
                &msgs,
                &defs,
                Some(&starter),
            )
            .await
        } else {
            client::stream_chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
        }
    };

    let outcome = agent::run_agent(chat, &cfg, &registry, &system, args.task.trim()).await?;
    match outcome.stop {
        // The final answer was already streamed to stdout during the call.
        StopReason::Done => {}
        StopReason::Divergence => eprintln!(
            "\n[stopped after {} steps: recent attempts added no new evidence; the answer above is the best result from established facts]",
            outcome.iters
        ),
        StopReason::MaxIters => eprintln!(
            "\n[stopped: step budget exhausted after {} steps, including the automatic continuations — the task may be incomplete]",
            outcome.iters
        ),
        StopReason::VerificationFailed => eprintln!(
            "\n[stopped: edits were made but verification never passed after {} steps]",
            outcome.iters
        ),
        // One-shot `aizen agent` is non-interactive: there is no next message to answer with, so
        // surface the question and exit rather than hang. Re-run in the REPL to answer it.
        StopReason::AwaitingInput(q) => eprintln!(
            "\n[the agent needs clarification — re-run interactively (`aizen`) to answer]\n❓ {q}"
        ),
        StopReason::Cancelled => eprintln!(
            "\n[stopped: cancelled by user after {} step(s)]",
            outcome.iters
        ),
        // A top-level run sets no wall-clock budget (the user is watching and owns Esc), so this is
        // effectively unreachable here — but the match must be total, and if a caller ever does set
        // one, saying "time" rather than "steps" is the difference between a useful message and a
        // misleading one.
        StopReason::Deadline => eprintln!(
            "\n[stopped: wall-clock budget reached after {} step(s) — the task may be incomplete]",
            outcome.iters
        ),
    }
    Ok(())
}

async fn run_workflow_cmd(args: WorkflowArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.spec)
        .with_context(|| format!("reading workflow spec {}", args.spec))?;
    let spec: agent::workflow::WorkflowSpec =
        serde_json::from_str(&text).context("parsing workflow spec JSON")?;

    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;
    let trace = args.trace.as_deref().map(std::path::Path::new);

    let approval = if args.yes {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Ask
    };
    agent::workflow::run_workflow(&http, &base_url, &api_key, &model, approval, &spec, trace).await
}

/// `aizen memory reconcile [--apply]` — the M2b batch pass, run by hand.
///
/// One model call, at most `MAX_PAIRS` pairs, and **dry-run by default**: the actions this pass
/// proposes overwrite bodies and retire facts, so the harmless mode has to be the one you get by
/// typing the short command. `--apply` is the sentence where the user says they read the dry run.
///
/// The call is routed through the summarizer role like every other chore call, so a cheap model can
/// own it without touching the main endpoint.
async fn run_memory_reconcile(apply: bool) -> Result<()> {
    let (pairs, live) = memory::reconcile_inputs()?;
    if pairs.is_empty() {
        crate::ui::tui::emit_line("no suspicious pairs — nothing to reconcile.");
        return Ok(());
    }
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    let ep = summarizer_endpoint(&base, &key, &model);

    // `judge` is the ONLY place this pass can reach a model, and it is `FnOnce` — the ≤1-call budget
    // is enforced by the type, not by remembering to not loop.
    let judge = |sys: &str, user: &str| -> Option<String> {
        let msgs = [
            Message::system(sys.to_string()),
            Message::user(user.to_string()),
        ];
        let fut = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
        // The surrounding fn is async, but `batch_pass` is sync (so every rail in it stays unit
        // testable without a runtime). Blocking here is safe: this is a one-shot CLI command with
        // nothing else on the runtime waiting on us.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(fut)
                .ok()?
                .content
        })
    };

    let report = memory::learning::reconcile::batch_pass(
        &pairs,
        judge,
        !apply,
        &memory::learning::default_session_id(),
        &live,
    );
    memory::print_reconcile_report(&report);
    Ok(())
}

async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Add {
            name,
            description,
            mtype,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading memory body from stdin")?,
            };
            if body.trim().is_empty() {
                anyhow::bail!("empty memory body (pass --body or pipe text on stdin)");
            }
            memory::cmd_add(&name, &description, &mtype, body.trim())
        }
        MemoryCmd::List {
            scope,
            scope_flag,
            superseded,
        } => {
            if superseded {
                memory::cmd_list_superseded()
            } else {
                memory::cmd_list(scope.or(scope_flag).as_deref())
            }
        }
        MemoryCmd::Revive { id } => memory::cmd_revive(&id),
        MemoryCmd::Show { id } => memory::cmd_show(&id),
        MemoryCmd::Search {
            query,
            k,
            dimension,
            category,
            scope,
        } => memory::cmd_search(&query, k, dimension, category, scope.as_deref()),
        MemoryCmd::Frozen { rebuild } => memory::cmd_frozen(rebuild),
        MemoryCmd::Learn { text, yes, dry_run } => {
            let text = match text {
                Some(t) => t,
                None => read_stdin("reading user turn from stdin")?,
            };
            if text.trim().is_empty() {
                anyhow::bail!("empty turn (pass text or pipe it on stdin)");
            }
            memory::cmd_learn(text.trim(), yes, dry_run)
        }
        MemoryCmd::Style => memory::cmd_style(),
        MemoryCmd::Profile { json } => memory::cmd_profile(json),
        MemoryCmd::Ask { question, json } => memory::cmd_ask(&question, json),
        MemoryCmd::Review {
            promote,
            drop_key,
            clear,
        } => memory::cmd_review(promote, drop_key, clear),
        MemoryCmd::AsOf { date } => memory::cmd_as_of(date.trim()),
        MemoryCmd::Supersede { old, new } => memory::cmd_supersede(&old, &new),
        MemoryCmd::Edit {
            id,
            name,
            description,
            mtype,
            body,
            scope,
        } => {
            // `--body -` reads the replacement body from stdin (so a multi-line rewrite can be piped
            // in); omitting `--body` entirely leaves the body untouched.
            let body = match body.as_deref() {
                Some("-") => Some(read_stdin("reading replacement body from stdin")?),
                _ => body,
            };
            memory::cmd_edit(&id, name, description, mtype, body, scope)
        }
        MemoryCmd::Forget { id } => memory::cmd_forget(&id),
        MemoryCmd::Purge { id, yes } => {
            if !yes {
                anyhow::bail!(
                    "`memory purge` permanently deletes an archived fact — pass --yes to confirm"
                );
            }
            memory::cmd_purge(&id)
        }
        MemoryCmd::Archive => memory::cmd_archive_list(),
        MemoryCmd::Restore { id, as_id } => memory::cmd_restore(&id, as_id.as_deref()),
        MemoryCmd::Compact => memory::cmd_compact(),
        MemoryCmd::Reconcile { apply } => run_memory_reconcile(apply).await,
        MemoryCmd::Doctor => memory::cmd_doctor(),
        MemoryCmd::Where => {
            println!("{}", memory_where_report());
            Ok(())
        }
        MemoryCmd::Health => memory::cmd_health(),
        MemoryCmd::Neighbors { id, k } => memory::cmd_neighbors(&id, k),
        MemoryCmd::ModelDownload { name } => memory::model_dl::download(name.as_deref())
            .await
            .map(|_| ()),
        MemoryCmd::ModelList => run_memory_model_list(),
    }
}

/// `aizen memory model-list` — show every model2vec model this machine already has, and which one
/// the dense tier would pick. Exists because the old failure mode was SILENT: with no model at the
/// configured name the loader fell back to the (non-semantic) hashing embedder, so a user who had
/// downloaded a perfectly good model under another name had no way to see why dense wasn't working.
fn run_memory_model_list() -> Result<()> {
    let configured = config::embed_model_name();
    let found = memory::embed::list_local_models();
    let chosen = memory::embed::discover_local_model();
    println!("configured model name: {configured}");
    println!("(override with AIZEN_EMBED_MODEL)");
    println!();
    if found.is_empty() {
        println!("no model2vec models found on this machine.");
        println!("  looked in: {}", config::models_dir().display());
        println!("             the Hugging Face hub cache (~/.cache/huggingface/hub, %LOCALAPPDATA%\\huggingface\\hub, $HF_HUB_CACHE)");
        println!();
        println!("get one with: aizen memory model-download");
        return Ok(());
    }
    println!(
        "found {} model2vec model{}:",
        found.len(),
        if found.len() == 1 { "" } else { "s" }
    );
    let chosen_dir = chosen.as_ref().map(|c| c.dir.clone());
    for c in &found {
        let marker = if Some(&c.dir) == chosen_dir.as_ref() {
            "▸"
        } else {
            " "
        };
        println!("  {marker} {} [{}]  {}", c.name, c.source, c.dir.display());
    }
    println!();
    match &chosen {
        Some(c) if c.name == configured => {
            println!("dense would load '{}' (the configured name).", c.name);
        }
        Some(c) => {
            println!(
                "dense would AUTO-DETECT '{}' from {} — '{configured}' is not present.",
                c.name, c.source
            );
        }
        None => println!("dense would fall back to the hashing embedder (not semantic)."),
    }
    // The weights only LOAD on a `--features dense` build; say so rather than implying the default
    // binary will use what we just listed.
    if cfg!(feature = "dense") {
        println!("this build has the dense feature: the model above will be loaded.");
    } else {
        println!(
            "note: this build has NO dense feature — rebuild with `--features dense` to use it."
        );
    }
    Ok(())
}

fn run_persona(cmd: PersonaCmd) -> Result<()> {
    match cmd {
        PersonaCmd::List => {
            let active_slug = cli_config::load()
                .persona
                .as_deref()
                .map(skill::sanitize_name);
            let ps = persona::list();
            if ps.is_empty() {
                println!("(no personas yet — `aizen persona new <name>`, or /persona in the REPL)");
                return Ok(());
            }
            for p in &ps {
                let slug = skill::sanitize_name(&p.name);
                let mark = if active_slug.as_deref() == Some(slug.as_str()) {
                    "●"
                } else {
                    "○"
                };
                let sub = if p.role.is_empty() {
                    p.voice.clone()
                } else {
                    p.role.clone()
                };
                let (eps, ins) = persona::self_mem::counts(&slug);
                println!(
                    "{mark} {} — {sub}  ({ins} insights, {eps} episodes)",
                    p.name
                );
            }
            // Retired cards are recoverable, so say they exist — a soft delete nobody can see is
            // indistinguishable from a hard one.
            let retired = persona::list_archive();
            if !retired.is_empty() {
                let names: Vec<&str> = retired.iter().map(|p| p.name.as_str()).collect();
                println!(
                    "\nretired: {} — `aizen persona restore <name>` brings one back",
                    names.join(", ")
                );
            }
            // Personas have no `where` sub-command of their own (one folder, no zoning), so the path
            // goes here directly — the card and its `.self/` memory are plain files worth editing.
            println!(
                "{}",
                style(format!(
                    "\nfiles: {}   (`<name>.md` is the card, `<name>.self/` its memory)",
                    persona::personas_dir().display()
                ))
                .dim()
            );
            Ok(())
        }
        PersonaCmd::Show { name } => {
            let p = persona::load(&name).ok_or_else(|| anyhow::anyhow!("no persona '{name}'"))?;
            println!("# {}", p.name);
            if !p.role.is_empty() {
                println!("role: {}", p.role);
            }
            if !p.voice.is_empty() {
                println!("voice: {}", p.voice);
            }
            if !p.body.is_empty() {
                println!("\n{}", p.body);
            }
            Ok(())
        }
        PersonaCmd::New {
            name,
            role,
            voice,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading persona body from stdin")?,
            };
            let path = persona::save(
                &name,
                role.as_deref().unwrap_or(""),
                voice.as_deref().unwrap_or(""),
                &body,
            )?;
            println!("saved persona → {}", path.display());
            Ok(())
        }
        PersonaCmd::Use { name } => {
            let p = persona::load(&name)
                .ok_or_else(|| anyhow::anyhow!("no persona '{name}' (see `aizen persona list`)"))?;
            let mut cfg = cli_config::load();
            cfg.persona = Some(p.name.clone());
            cli_config::save(&cfg)?;
            println!("now playing: {}", p.name);
            Ok(())
        }
        PersonaCmd::Clear => {
            let mut cfg = cli_config::load();
            cfg.persona = None;
            cli_config::save(&cfg)?;
            println!("persona cleared → default assistant voice");
            Ok(())
        }
        PersonaCmd::SelfMem { name, all } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass a name or `aizen persona use <name>`")
                })?,
            };
            let label = persona::load(&slug)
                .map(|p| p.name)
                .unwrap_or_else(|| slug.clone());
            persona_self_view_n(&slug, &label, all);
            Ok(())
        }
        PersonaCmd::Forget { id, name } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass --name or `aizen persona use <name>`")
                })?,
            };
            persona::self_mem::forget(&slug, &id)?;
            println!(
                "retired self-memory '{id}' (archived — `aizen persona unforget {id}` restores it)"
            );
            Ok(())
        }
        PersonaCmd::Unforget { id, name } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass --name or `aizen persona use <name>`")
                })?,
            };
            persona::self_mem::restore(&slug, &id)?;
            println!("restored self-memory '{id}'");
            Ok(())
        }
        PersonaCmd::Remember { text, importance } => {
            let slug = persona::active_slug().ok_or_else(|| {
                anyhow::anyhow!("no active persona — `aizen persona use <name>` first")
            })?;
            // Explicit CLI remember is always formative: force Explicit kind + floor ≥ FORMATIVE_MIN.
            let imp = importance
                .unwrap_or_else(|| {
                    persona::self_mem::classify_turn(&text, 0)
                        .map(|s| s.importance)
                        .unwrap_or(6)
                })
                .max(persona::self_mem::FORMATIVE_MIN)
                .min(10);
            let body = if text.trim().starts_with("correction:")
                || text.trim().starts_with("preference:")
                || text.trim().starts_with("work:")
                || text.trim().starts_with("bond:")
                || text.trim().starts_with("explicit:")
            {
                text.clone()
            } else {
                persona::self_mem::format_episode_body(
                    persona::self_mem::EventKind::Explicit,
                    &text,
                    0,
                    "",
                )
            };
            match persona::self_mem::record_episode(&slug, &body, imp)? {
                Some(id) => println!("recorded episode '{id}' (importance {imp})"),
                None => println!("(skipped — near-duplicate of a recent episode/insight)"),
            }
            Ok(())
        }
        PersonaCmd::Delete { name } => {
            if persona::delete(&name)? {
                // Clear the active pointer too, or the config keeps naming a card that is gone.
                let mut cfg = cli_config::load();
                if cfg
                    .persona
                    .as_deref()
                    .map(|p| skill::sanitize_name(p) == skill::sanitize_name(&name))
                    .unwrap_or(false)
                {
                    cfg.persona = None;
                    cli_config::save(&cfg)?;
                    println!("retired persona '{name}' (was active — back to the default voice)");
                } else {
                    println!("retired persona '{name}'");
                }
                println!(
                    "card + self-memory archived under {} — `aizen persona restore {name}`",
                    persona::archive_dir().display()
                );
            } else {
                println!("no persona named '{name}'");
            }
            Ok(())
        }
        PersonaCmd::Restore { name } => {
            let p = persona::restore(&name)?;
            println!("restored persona '{name}' → {}", p.display());
            Ok(())
        }
        PersonaCmd::Block => {
            match persona::prompt_block() {
                Some(p) => println!("<persona>\n{}\n</persona>", p.trim()),
                None => {
                    println!("(no persona active — `aizen persona use <name>`)");
                    return Ok(());
                }
            }
            match persona::self_block() {
                Some(s) => println!("\n<self>\n{}\n</self>", s.trim()),
                None => println!("\n(no <self> yet — the character has no self-memory; `aizen persona remember \"...\"`)"),
            }
            Ok(())
        }
    }
}

fn run_soul(cmd: Option<SoulCmd>) -> Result<()> {
    match cmd.unwrap_or(SoulCmd::Show) {
        SoulCmd::Show => {
            match soul::prompt_block() {
                Some(b) => println!("<agent_identity>\n{}\n</agent_identity>", b.trim()),
                None if soul::exists() => println!(
                    "(SOUL.md exists but renders nothing — it is empty or was dropped by the safety \
                     scan; see {})",
                    soul::soul_path().display()
                ),
                None => println!(
                    "(no operating identity yet — set one with `aizen soul set` or edit {})",
                    soul::soul_path().display()
                ),
            }
            Ok(())
        }
        SoulCmd::Set { body } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading SOUL body from stdin")?,
            };
            let path = soul::write(&body)?;
            println!("saved operating identity → {}", path.display());
            if soul::prompt_block().is_none() {
                println!(
                    "{}",
                    style("⚠ heads up: it renders nothing — the safety scan dropped it (a credential or \
                     injection-looking line). It will NOT be injected until fixed.")
                        .yellow()
                );
            }
            Ok(())
        }
        SoulCmd::Clear => {
            if soul::clear()? {
                println!("operating identity cleared");
            } else {
                println!("(no operating identity to clear)");
            }
            Ok(())
        }
        SoulCmd::Path => {
            println!("{}", soul::soul_path().display());
            Ok(())
        }
    }
}

async fn run_skill(cmd: SkillCmd) -> Result<()> {
    match cmd {
        SkillCmd::List { all_zones } => {
            let skills = skill::list();
            if skills.is_empty() && !all_zones {
                println!(
                    "(no skills — add one with `aizen skill add <name>`, or /skills in the REPL)"
                );
                return Ok(());
            }
            // Aligned columns, and the usage count as the triage signal: a skill the agent has never
            // loaded is either mis-triggered (`when:` doesn't match how the task gets phrased) or not
            // needed. Unaligned `name — description` gave no way to compare that across rows.
            let namew = skills
                .iter()
                .map(|s| s.name.chars().count())
                .max()
                .unwrap_or(0)
                .min(38);
            for s in &skills {
                let d = if s.description.is_empty() {
                    &s.when
                } else {
                    &s.description
                };
                let tag = match s.origin {
                    skill::SkillOrigin::Global => "",
                    skill::SkillOrigin::Project => " [project]",
                    skill::SkillOrigin::Repo => " [repo]",
                };
                let uses = if s.uses > 0 {
                    format!("{}×", s.uses)
                } else {
                    "cold".to_string()
                };
                let pad = namew.saturating_sub(s.name.chars().count());
                println!(
                    "  {}{}  {:<5} {}{tag}",
                    s.name,
                    " ".repeat(pad),
                    uses,
                    elide(d, 92)
                );
            }
            let cold = skills.iter().filter(|s| s.uses == 0).count();
            println!("\n{} skill(s)", skills.len());
            if cold > 0 {
                println!(
                    "{}",
                    style(format!(
                        "{cold} cold (never loaded — check `when:` matches how you'd phrase the task)"
                    ))
                    .dim()
                );
            }
            println!(
                "{}",
                style(
                    "`skill show <name>` reads one · `skill refine <name>` rewrites the steps · \
                     `skill delete <name>` retires (restorable)\n\
                     `skill where` prints the three folders skills are read from"
                )
                .dim()
            );
            if all_zones {
                let others = skill::list_other_zones();
                if !others.is_empty() {
                    println!("\nother workspaces' zones (invisible here):");
                    for (zone, s) in &others {
                        println!("  {}  [p:{zone}]  {}", s.name, elide(&s.description, 80));
                    }
                }
            }
            let retired = skill::list_archive();
            if !retired.is_empty() {
                let names: Vec<&str> = retired.iter().map(|s| s.name.as_str()).collect();
                println!(
                    "\nretired: {} — `aizen skill restore <name>` brings one back",
                    names.join(", ")
                );
            }
            Ok(())
        }
        SkillCmd::Show { name } => match skill::load(&name) {
            Some(sk) => {
                println!("{}", skill::render_loaded(&sk));
                Ok(())
            }
            None => anyhow::bail!("no skill named '{name}' (try `aizen skill list`)"),
        },
        SkillCmd::Where => {
            println!("{}", skill_where_report());
            Ok(())
        }
        SkillCmd::Add {
            name,
            description,
            when,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading skill body from stdin")?,
            };
            let path = skill::save(
                &name,
                description.as_deref().unwrap_or(""),
                when.as_deref().unwrap_or(""),
                &body,
            )?;
            println!("saved skill → {}", path.display());
            Ok(())
        }
        SkillCmd::Delete { name } => {
            if skill::delete(&name)? {
                println!(
                    "retired '{name}' (archived — `aizen skill restore {name}` brings it back)"
                );
            } else {
                println!("(no skill named '{name}')");
            }
            Ok(())
        }
        SkillCmd::Restore { name } => {
            let p = skill::restore(&name)?;
            println!("restored '{name}' → {}", p.display());
            Ok(())
        }
        SkillCmd::Refine {
            name,
            description,
            when,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading the refined skill body from stdin")?,
            };
            let (version, archived) =
                skill::refine(&name, &body, description.as_deref(), when.as_deref())?;
            println!(
                "{} '{name}' → v{version} (prior version archived at {})",
                style("refined").color256(splash::ACCENT),
                archived.display()
            );
            Ok(())
        }
        SkillCmd::Fetch { url, name } => run_skill_fetch(&url, name.as_deref()).await,
        SkillCmd::Search { query, limit } => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                anyhow::bail!(
                    "a search query is required, e.g. `aizen skill search deploy fastapi`"
                );
            }
            let hits = skill_registry::search(&q, limit.unwrap_or(20).clamp(1, 50)).await?;
            if hits.is_empty() {
                println!(
                    "no skills on {} match '{q}'",
                    skill_registry::registry_base()
                );
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "{} result(s) from {} — install with `aizen skill install <owner/name>`",
                    hits.len(),
                    skill_registry::registry_base()
                ))
                .dim()
            );
            for sk in &hits {
                println!("{}", sk.summary_line());
            }
            Ok(())
        }
        SkillCmd::Install { slug } => {
            let sk = skill_registry::install(&slug).await?;
            println!(
                "{} '{}' → {}",
                style("installed").color256(splash::ACCENT),
                sk.name,
                skill::skills_dir()
                    .join(format!("{}.md", skill::sanitize_name(&sk.name)))
                    .display()
            );
            Ok(())
        }
    }
}

/// GET a markdown skill from `url` and save it (name from `--name` > frontmatter > URL filename).
/// SSRF-guarded like every other outbound fetch: the URL passes the net_guard floor and the fetch
/// goes through the shared guarded client (no auto-redirects; every hop re-vetted; body bounded).
async fn run_skill_fetch(url: &str, name_override: Option<&str>) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("fetch needs an absolute http(s) URL");
    }
    crate::core::net_guard::guard_url_async(url).await?;
    let http = crate::agent::reach::http::client()?;
    let resp = crate::agent::reach::http::get(&http, url, &[])
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.is_success() {
        anyhow::bail!("upstream returned HTTP {}", resp.status);
    }
    let text = resp.text();
    // Fallback name from the URL's filename (strip a trailing .md).
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("skill");
    let stem = stem
        .split(['?', '#'])
        .next()
        .unwrap_or(stem)
        .trim_end_matches(".md");
    let sk = skill::parse_markdown(&text, stem);
    let name = name_override.unwrap_or(&sk.name);
    let path = skill::save(name, &sk.description, &sk.when, &sk.body)?;
    println!("fetched skill '{name}' → {}", path.display());
    Ok(())
}

fn read_stdin(ctx: &'static str) -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context(ctx)?;
    // Strip a leading UTF-8 BOM (PowerShell's `|` prepends one) before trimming.
    Ok(buf
        .strip_prefix('\u{FEFF}')
        .unwrap_or(&buf)
        .trim()
        .to_string())
}

// ── `aizen agents …` — the specialist sub-agent library (agency-agents format) ──

/// A classified install source for `aizen agents install`.
#[derive(Debug, PartialEq)]
enum InstallSource {
    /// `owner/repo` → cloned from github.com.
    GitHubShorthand(String),
    /// A full git URL (https `.git`/repo, `git@…`, `ssh://…`).
    GitUrl(String),
    /// A single `.md` agent file fetched over http(s).
    FileUrl(String),
    /// A local directory tree.
    LocalDir(std::path::PathBuf),
}

/// Classify an install source string. Pure (no IO except an existing-dir probe) so it's unit-testable.
fn classify_source(raw: &str) -> Result<InstallSource> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("an install source is required (owner/repo, a git/.md URL, or a local dir)");
    }
    // http(s): a single .md file vs a git repo.
    if s.starts_with("http://") || s.starts_with("https://") {
        let path_only = s.split(['?', '#']).next().unwrap_or(s);
        if path_only.to_ascii_lowercase().ends_with(".md") {
            return Ok(InstallSource::FileUrl(s.to_string()));
        }
        return Ok(InstallSource::GitUrl(s.to_string()));
    }
    // scp-like / ssh git URLs, or any bare `*.git`.
    if s.starts_with("git@") || s.starts_with("ssh://") || s.ends_with(".git") {
        return Ok(InstallSource::GitUrl(s.to_string()));
    }
    // Explicit local-path forms, a Windows drive (`C:\…`), or an existing directory.
    let drive = {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let looks_local = s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with(".\\")
        || s.starts_with("..\\")
        || s.starts_with('/')
        || s.starts_with('\\')
        || drive;
    if looks_local || std::path::Path::new(s).is_dir() {
        return Ok(InstallSource::LocalDir(std::path::PathBuf::from(s)));
    }
    // GitHub shorthand: exactly `owner/repo` (one slash, both halves non-empty, no whitespace).
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && !p.contains(char::is_whitespace))
    {
        return Ok(InstallSource::GitHubShorthand(s.to_string()));
    }
    anyhow::bail!(
        "unrecognized source '{raw}' — use owner/repo, an https git URL, a `.md` URL, or a local directory"
    );
}

async fn run_agents(cmd: Option<AgentsCmd>) -> Result<()> {
    match cmd {
        None => {
            agents_default_view();
            Ok(())
        }
        Some(AgentsCmd::List {
            division,
            source,
            enabled,
            json,
        }) => agents_list(division.as_deref(), source.as_deref(), enabled, json),
        Some(AgentsCmd::Show { name }) => match agents::load(&name) {
            Some(def) => {
                println!("{}", agents::render_card(&def));
                Ok(())
            }
            None => anyhow::bail!("no agent named '{name}' (try `aizen agents list`)"),
        },
        Some(AgentsCmd::Where) => {
            agents_where();
            Ok(())
        }
        Some(AgentsCmd::Install {
            source,
            yes,
            enable_all,
            as_name,
        }) => agents_install(&source, yes, enable_all, as_name.as_deref()).await,
        Some(AgentsCmd::Remove { name }) => {
            if agents::delete_home(&name)? {
                let _ = agents::set_enabled(&name, false);
                println!("removed '{name}' from ~/.aizen/agents and unpinned it");
            } else {
                println!("(no agent named '{name}' under ~/.aizen/agents — `aizen agents where` shows the dirs)");
            }
            Ok(())
        }
        Some(AgentsCmd::Enable { name, all }) => agents_set_enabled(name.as_deref(), all, true),
        Some(AgentsCmd::Disable { name, all }) => agents_set_enabled(name.as_deref(), all, false),
        Some(AgentsCmd::SetProvider {
            name,
            provider,
            model,
            clear,
        }) => {
            let mut cfg = cli_config::load();
            if agents::load(&name).is_none() {
                anyhow::bail!("no agent named '{name}' (try `aizen agents list`)");
            }
            if clear {
                cfg.set_agent_route(&name, None, None)?;
                cli_config::save(&cfg)?;
                println!(
                    "{} cleared provider assignment on '{}'",
                    crate::ui::theme::ok("✓"),
                    name
                );
            } else {
                let provider = provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("pass a saved provider name (or --clear)")?;
                cfg.set_agent_route(&name, Some(provider.to_string()), model)?;
                cli_config::save(&cfg)?;
                let route = cfg.agent_route(&name).expect("route just saved");
                println!(
                    "{} assigned '{}' to provider {} · model {}",
                    crate::ui::theme::ok("✓"),
                    name,
                    route.provider.as_deref().unwrap_or("inherit"),
                    route.model.as_deref().unwrap_or("provider default")
                );
            }
            Ok(())
        }
        Some(AgentsCmd::SetModel { name, model, clear }) => {
            let new_model = if clear {
                None
            } else {
                match model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(m) => Some(m.to_string()),
                    None => anyhow::bail!("pass a model id (or --clear to remove the pin)"),
                }
            };
            let path = agents::set_model(&name, new_model.as_deref())?;
            match &new_model {
                Some(m) => println!(
                    "{} pinned '{name}' to model {} ({})",
                    crate::ui::theme::ok("✓"),
                    style(m).color256(splash::ACCENT),
                    path.display()
                ),
                None => println!(
                    "{} cleared the model pin on '{name}' ({})",
                    crate::ui::theme::ok("✓"),
                    path.display()
                ),
            }
            Ok(())
        }
    }
}

/// Bare `aizen agents`: list when any exist, else the install nudge.
fn agents_default_view() {
    if agents::has_any() {
        let _ = agents_list(None, None, false, false);
    } else {
        agents_nudge();
    }
}

fn agents_nudge() {
    println!("No specialist agents yet. Install the agency-agents library with:");
    println!(
        "  {}",
        style("aizen agents install msitarzewski/agency-agents").color256(splash::ACCENT)
    );
    println!("…or drop `.md` personas into ~/.aizen/agents (or ~/.claude/agents).");
}

fn agents_list(
    division: Option<&str>,
    source: Option<&str>,
    enabled_only: bool,
    json: bool,
) -> Result<()> {
    let enabled = agents::enabled_set();
    let is_enabled = |slug: &str| enabled.as_ref().map(|e| e.contains(slug)).unwrap_or(false);

    let mut all = agents::list();
    if let Some(d) = division {
        let d = d.to_lowercase();
        all.retain(|a| a.division.as_deref() == Some(d.as_str()));
    }
    if let Some(src) = source {
        let src = src.to_lowercase();
        all.retain(|a| a.source.label() == src);
    }
    if enabled_only {
        all.retain(|a| is_enabled(&a.slug()));
    }

    let cfg = cli_config::load();
    if json {
        let arr: Vec<serde_json::Value> = all
            .iter()
            .map(|a| {
                serde_json::json!({
                    "slug": a.slug(),
                    "name": a.name,
                    "description": a.description,
                    "division": a.division,
                    "source": a.source.label(),
                    "model": a.model,
                    "provider": cfg.agent_route(&a.slug()).and_then(|r| r.provider.clone()),
                    "route_model": cfg.agent_route(&a.slug()).and_then(|r| r.model.clone()),
                    "tools": a.tools,
                    "enabled": is_enabled(&a.slug()),
                    "path": a.source_path.display().to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if all.is_empty() {
        if agents::has_any() {
            println!("(no agents match the filter)");
        } else {
            agents_nudge();
        }
        return Ok(());
    }

    let mut by_div: std::collections::BTreeMap<String, Vec<&agents::AgentDef>> =
        std::collections::BTreeMap::new();
    for a in &all {
        by_div
            .entry(
                a.division
                    .clone()
                    .unwrap_or_else(|| "(no division)".to_string()),
            )
            .or_default()
            .push(a);
    }
    let total = all.len();
    let enabled_count = all.iter().filter(|a| is_enabled(&a.slug())).count();
    for (div, items) in &by_div {
        println!("{}", style(format!("{div} ({})", items.len())).bold());
        for a in items {
            let mark = if is_enabled(&a.slug()) {
                style("●").color256(splash::ACCENT).to_string()
            } else {
                style("○").dim().to_string()
            };
            let desc: String = a.description.chars().take(80).collect();
            let route = cfg.agent_route(&a.slug());
            let route_hint = route
                .map(|r| {
                    format!(
                        " · provider {} · {}",
                        r.provider.as_deref().unwrap_or("inherit"),
                        r.model.as_deref().unwrap_or("default model")
                    )
                })
                .unwrap_or_default();
            println!(
                "  {} {}  —  {}{}",
                mark,
                a.slug(),
                desc.replace('\n', " "),
                route_hint
            );
        }
    }
    let hint = if enabled.is_some() {
        format!("{total} agent(s) · {enabled_count} pinned to <agents>. Dispatch: task(agent=\"<slug>\").")
    } else {
        format!("{total} agent(s) · none pinned — `aizen agents enable <slug>` to advertise them.")
    };
    println!("{}", style(hint).dim());
    Ok(())
}

fn agents_where() {
    println!("Specialist agent sources (lower → higher precedence):");
    for (src, dir, n) in agents::source_counts() {
        let status = if !dir.exists() {
            style("(absent)").dim().to_string()
        } else {
            format!("{n} agent(s)")
        };
        println!("  {:<16} {}  [{}]", src.label(), dir.display(), status);
    }
    println!(
        "{}",
        style(
            "Installs write to ~/.aizen/agents; a higher-precedence dir wins on a slug collision."
        )
        .dim()
    );
}

fn agents_set_enabled(name: Option<&str>, all: bool, on: bool) -> Result<()> {
    if all {
        agents::set_all_enabled(on)?;
        println!(
            "{} all agents {} the <agents> index",
            if on { "pinned" } else { "unpinned" },
            if on { "to" } else { "from" }
        );
        return Ok(());
    }
    let name = name.context("provide an agent name, or pass --all")?;
    let def = agents::load(name)
        .with_context(|| format!("no agent named '{name}' (try `aizen agents list`)"))?;
    agents::set_enabled(&def.slug(), on)?;
    println!(
        "{} '{}' {} the <agents> index",
        if on { "pinned" } else { "unpinned" },
        def.slug(),
        if on { "to" } else { "from" }
    );
    Ok(())
}

fn confirm_write(prompt: &str) -> Result<bool> {
    Ok(Confirm::with_theme(&ui_theme())
        .with_prompt(prompt)
        .default(true)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false))
}

/// A filesystem-safe directory name from a repo slug/URL (last segment, `.git` stripped).
fn sanitize_repo_name(s: &str) -> String {
    let base = s
        .trim_end_matches('/')
        .rsplit(['/', ':', '\\'])
        .next()
        .unwrap_or(s);
    let base = base.trim_end_matches(".git");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() {
        "agents".to_string()
    } else {
        cleaned
    }
}

fn unique_n() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// `git clone --depth 1` (shallow, quiet, NO submodule recursion) into `dest`. Cloning runs NO repo
/// code — it only reads files. `--no-recurse-submodules` stops a hostile `.gitmodules` from making git
/// fetch arbitrary (possibly internal) submodule URLs we never vetted.
fn git_clone_shallow(url: &str, dest: &std::path::Path) -> Result<()> {
    let out = crate::core::gitx::command()?
        .args([
            "clone",
            "--depth",
            "1",
            "--no-recurse-submodules",
            "--quiet",
            url,
        ])
        .arg(dest)
        .output()
        .context("running `git clone` (is git installed and on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Extract the host from a NON-http(s) git URL (`git@host:path`, `ssh://[user@]host[:port]/path`,
/// `git://host/path`) so the SSRF floor can guard it too. `None` if no host is discernible.
fn git_url_host(url: &str) -> Option<String> {
    let non_empty = |s: &str| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    // scp-like: [user@]host:path (no scheme).
    if url.starts_with("git@") || (url.contains('@') && url.contains(':') && !url.contains("://")) {
        let after_at = url.rsplit('@').next().unwrap_or(url);
        return non_empty(after_at.split(':').next().unwrap_or(after_at));
    }
    let rest = url
        .strip_prefix("ssh://")
        .or_else(|| url.strip_prefix("git://"))?;
    let after_at = rest.rsplit('@').next().unwrap_or(rest);
    non_empty(after_at.split(['/', ':']).next().unwrap_or(after_at))
}

/// Copy every `*.md` that `looks_like_agent` from `src` (recursively, dotdirs skipped) into
/// `dest_root`, preserving the relative subpath. Returns `(copied, skipped)`.
fn copy_agent_tree(src: &std::path::Path, dest_root: &std::path::Path) -> Result<(usize, usize)> {
    let mut copied = 0;
    let mut skipped = 0;
    copy_agent_walk(src, src, dest_root, &mut copied, &mut skipped, 0)?;
    Ok((copied, skipped))
}

fn copy_agent_walk(
    dir: &std::path::Path,
    src_root: &std::path::Path,
    dest_root: &std::path::Path,
    copied: &mut usize,
    skipped: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > 12 {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip .git, dotfiles
        }
        // Never follow symlinks out of an UNTRUSTED cloned tree (a symlinked `x.md` could pull in a
        // file outside the repo; a symlinked dir could escape it). The loader treats the user's own
        // dirs as trusted, but install copies third-party content, so refuse symlinks here.
        if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_agent_walk(&p, src_root, dest_root, copied, skipped, depth + 1)?;
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        {
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            if !agents::looks_like_agent(&content) {
                *skipped += 1;
                continue;
            }
            let rel = p.strip_prefix(src_root).unwrap_or(&p);
            let dest = dest_root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&dest, content)
                .with_context(|| format!("writing {}", dest.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

async fn agents_install(
    source: &str,
    yes: bool,
    enable_all: bool,
    as_name: Option<&str>,
) -> Result<()> {
    let classified = classify_source(source)?;
    println!(
        "{}",
        style("⚠ agent bodies are third-party system prompts — they run as sub-agents with edit/shell scope. Review before pinning.").dim()
    );
    match classified {
        InstallSource::FileUrl(url) => {
            crate::core::net_guard::guard_url_async(&url).await?;
            if !yes && !confirm_write(&format!("Fetch and install the agent at {url}?"))? {
                println!("cancelled.");
                return Ok(());
            }
            // Fetch through the shared guarded client (auto-redirects OFF, every hop re-vetted
            // against the net_guard floor) — a plain reqwest client follows up to 10 redirects and
            // would re-vet only the first hop, so a 302 → 169.254.169.254 / localhost slips through.
            let http = crate::agent::reach::http::client()?;
            let resp = crate::agent::reach::http::get(&http, &url, &[])
                .await
                .with_context(|| format!("GET {url}"))?;
            if !resp.is_success() {
                anyhow::bail!("upstream returned HTTP {}", resp.status);
            }
            let text = resp.text();
            if !agents::looks_like_agent(&text) {
                anyhow::bail!(
                    "that URL isn't an agent (needs frontmatter `name:` + a non-empty body)"
                );
            }
            let stem = url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("agent");
            let stem = stem
                .split(['?', '#'])
                .next()
                .unwrap_or(stem)
                .trim_end_matches(".md");
            let stem = if stem.is_empty() { "agent" } else { stem }; // e.g. a URL ending in "/.md"
            let path = agents::save_home(&text, as_name.unwrap_or(stem))?;
            println!("installed 1 agent → {}", path.display());
            if enable_all {
                agents::set_all_enabled(true)?;
                println!("…and pinned all agents to <agents>.");
            }
            Ok(())
        }
        InstallSource::LocalDir(dir) => {
            if !dir.is_dir() {
                anyhow::bail!("not a directory: {}", dir.display());
            }
            if !yes && !confirm_write(&format!("Install agents from {}?", dir.display()))? {
                println!("cancelled.");
                return Ok(());
            }
            let label = dir.file_name().and_then(|s| s.to_str()).unwrap_or("local");
            let dest = agents::agents_dir().join(sanitize_repo_name(label));
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            let (copied, skipped) = copy_agent_tree(&dir, &dest)?;
            crate::core::config::harden_dir(&agents::agents_dir());
            finish_install(copied, skipped, &dest, enable_all)
        }
        InstallSource::GitHubShorthand(slug) => {
            install_from_git(
                &format!("https://github.com/{slug}.git"),
                &slug,
                yes,
                enable_all,
            )
            .await
        }
        InstallSource::GitUrl(url) => {
            let label = sanitize_repo_name(&url);
            install_from_git(&url, &label, yes, enable_all).await
        }
    }
}

async fn install_from_git(url: &str, label: &str, yes: bool, enable_all: bool) -> Result<()> {
    // SSRF floor — guard the destination host whatever the git transport is.
    if url.starts_with("https://") || url.starts_with("http://") {
        crate::core::net_guard::guard_url_async(url).await?;
    } else if let Some(host) = git_url_host(url) {
        // ssh:// / git@ / git:// never go through the http(s) guard; guard the resolved host directly
        // so an internal endpoint (e.g. git@10.0.0.5:…) can't be reached past the floor.
        crate::core::net_guard::guard_url_async(&format!("https://{host}")).await?;
    }
    if !yes
        && !confirm_write(&format!(
            "Clone {url} and install its agents into ~/.aizen/agents?"
        ))?
    {
        println!("cancelled.");
        return Ok(());
    }
    let repo = sanitize_repo_name(label);
    let dest = agents::agents_dir().join(&repo);
    let staging = std::env::temp_dir().join(format!(
        "aizen-agents-clone-{}-{}",
        std::process::id(),
        unique_n()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    println!("{}", style(format!("cloning {url} …")).dim());

    let url_s = url.to_string();
    let staging_c = staging.clone();
    let clone_res = tokio::task::spawn_blocking(move || git_clone_shallow(&url_s, &staging_c))
        .await
        .context("clone task panicked")?;

    // Always clean the staging clone, whether or not the copy succeeds.
    let outcome = (|| -> Result<(usize, usize)> {
        clone_res?;
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        let counts = copy_agent_tree(&staging, &dest)?;
        crate::core::config::harden_dir(&agents::agents_dir());
        Ok(counts)
    })();
    let _ = std::fs::remove_dir_all(&staging);

    let (copied, skipped) = outcome?;
    finish_install(copied, skipped, &dest, enable_all)
}

fn finish_install(
    copied: usize,
    skipped: usize,
    dest: &std::path::Path,
    enable_all: bool,
) -> Result<()> {
    if copied == 0 {
        // Nothing landed. Use `remove_dir` (empty-only), NOT `remove_dir_all`: it removes the dir we
        // just created but can never wipe pre-existing user files in a same-named directory.
        let _ = std::fs::remove_dir(dest);
        anyhow::bail!(
            "no agents found ({skipped} non-agent file(s) skipped) — the source had no `*.md` with frontmatter `name:` + a body"
        );
    }
    println!(
        "installed {copied} agent(s) → {} ({skipped} non-agent file(s) skipped)",
        dest.display()
    );
    if enable_all {
        agents::set_all_enabled(true)?;
        println!("…and pinned all agents to <agents>.");
    } else {
        println!(
            "{}",
            style("none are pinned yet — `aizen agents enable <slug>` (or re-run with --enable-all) to advertise them.").dim()
        );
    }
    println!(
        "{}",
        style("review: `aizen agents list` · `aizen agents show <slug>`").dim()
    );
    Ok(())
}

#[cfg(test)]
#[path = "tests/main_suite.rs"]
mod tests;
