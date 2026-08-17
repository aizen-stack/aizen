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
mod cli;
mod cli_args; // one module per `aizen <subcommand>` — the behaviour behind `cli_args`

// The reorg moved 23 top-level files into the folders above. These re-exports keep the
// call sites in THIS file referring to the modules by their short names (no behavior
// change) — every other file already uses the new `crate::<group>::<mod>` paths.
use crate::agent::prompt_lanes::*;
// Subcommand bodies. Glob-imported so the dispatch in `main` keeps calling them by bare name, and
// so `src/tests/main_suite.rs` (a `#[path]` child of the crate root) still resolves them through
// `use super::*` — the same arrangement the earlier extractions use. `pub(crate)` because these
// names USED to live at the crate root, and `src/ui/menus.rs` reaches them through `use crate::*`;
// re-exporting keeps that reachability byte-identical to before the move.
pub(crate) use crate::cli::read_stdin;
pub(crate) use crate::cli::{
    agents_cmd::*, apps::*, persona_cmd::*, sessions::*, skill_cmd::*, time::*, where_report::*,
};
use crate::core::session_store::*;
use crate::core::{cli_config, config, types};
use crate::features::slash_handlers::{
    handle_slash, slash_is_interactive, slash_menu, team_status_lines, SlashOutcome,
};
use crate::features::{commands, coop, crawl, cron};
use crate::hostbot::platforms::telegram;
use crate::llm::client;
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::context_report::*;
pub(crate) use crate::ui::effort_ui::*;
use crate::ui::menus::{apps_menu, run_discord, run_telegram};
pub(crate) use crate::ui::plain_input::read_input_box;
use crate::ui::{config_ui, icons, image_input, splash, theme, tui};

// The `clap` type tree (every subcommand enum) lives in its own file — see `cli_args`.
use cli_args::*;

use crate::core::approval::ApprovalMode;
use agent::{AgentConfig, AgentOutcome, StopReason};
use anyhow::{bail, Context, Result};
use clap::Parser;
use console::{style, Style};
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
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

    // Same lanes a turn would send: static base + <environment> (stable) and the memory/persona/
    // tool-routing blocks (dynamic). LSP is armed first so the registry advertises the symbolic-edit
    // tools; the registry is built BEFORE the lanes because it publishes the tool surface the routing
    // map is generated from — measuring the lanes first would under-report the real per-turn cost.
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
    let bundle = active_system_prompt_bundle(&model);
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
    // Registry BEFORE prompt: building it publishes this session's live tool surface, which is what
    // the prompt's tool-routing map is generated from. Assembling the prompt first would emit a
    // routing map for whatever surface a previous run left published — or none at all on the first.
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        resolve_ctx_window(model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        model,
        Some(&frozen),
    );
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
pub(crate) async fn cancellable_slash<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
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
                let persona_before = cli_config::load().persona;
                // Arm LSP BEFORE building the registry — tools only register while enabled.
                arm_lsp_session();
                // Registry BEFORE the user message is seated: building it publishes this turn's live
                // tool surface, and `seat_user_message` rewrites the dynamic prompt lane, which is
                // where the routing map generated from that surface lives. Seating first would ship a
                // map for the PREVIOUS turn's surface (and none at all on the first turn after
                // `/lsp on`, `/apps` or a `/tools` change). Nothing here reads `history`, so the move
                // is behaviour-neutral for everything else; on failure nothing has been pushed yet.
                let registry = match build_turn_registry(&http, &ep) {
                    Ok(r) => r,
                    Err(e) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        continue;
                    }
                };
                // Fold memory recall + codebase RAG into the SENT content (not the dynamic system
                // lane) so index 1 stays byte-stable and the transcript-tail prefix cache holds.
                // `line` itself is unchanged → checkpoint / display / persisted history keep the
                // clean user text.
                seat_user_message(&line, images, &mut history, &model);
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
        // Snapshot the active persona so we can detect an in-turn switch (the `persona_create` tool)
        // and resync the system prompt at the turn boundary — prefix-cache safe, takes effect next msg.
        let persona_before = cli_config::load().persona;
        arm_lsp_session();
        // Registry BEFORE the user message is seated — it publishes the live tool surface that
        // `seat_user_message`'s dynamic-lane rewrite turns into the routing map. Same ordering as the
        // retained REPL above, for the same reason.
        let registry = match build_turn_registry(&http, &ep) {
            Ok(r) => r,
            Err(e) => {
                tui::note_line(&format!("{} {e}", style("error:").red()));
                continue;
            }
        };
        // Fold memory recall + codebase-index retrieval into the SENT content (not the cached
        // system lane) — see `fold_context_into_query`. `line` stays the original for persisted
        // history / display.
        seat_user_message(&line, images, &mut history, &model);
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
pub(crate) async fn chore_chat(
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
pub(crate) fn approval_mode() -> ApprovalMode {
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
pub(crate) fn persona_evolve_enabled() -> bool {
    cli_config::load().persona_evolve.unwrap_or(true)
}

/// Pull the first top-level JSON object out of a model reply (tolerating ```json fences / prose).
pub(crate) fn extract_json_object(s: &str) -> Option<&str> {
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
pub(crate) fn print_status_line(history: &[Message], model: &str) {
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
pub(crate) fn elide(s: &str, max: usize) -> String {
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
pub(crate) async fn compact_now(history: &mut Vec<Message>) -> Result<(usize, usize)> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    compact_history(history, &http, &base, &key, &model).await
}

/// `/handoff` — one goal-conditioned extraction call over the current history (routed through the
/// summarizer role, like compaction). Returns the extraction; the caller rebuilds the thread.
pub(crate) async fn handoff_now(history: &[Message], goal: &str) -> Result<String> {
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

    // Registry includes the `task` sub-agent tool (depth 0); a spawned sub-agent uses a
    // role-scoped registry WITHOUT `task` (no recursion).
    let cli_approval = if args.yes {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Ask
    };
    arm_lsp_session();
    // Built BEFORE the prompt: it publishes the live tool surface the routing map is generated from.
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        cli_approval,
        resolve_ctx_window(&model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        &model,
        Some(&frozen),
    );
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

#[cfg(test)]
#[path = "tests/main_suite.rs"]
mod tests;
