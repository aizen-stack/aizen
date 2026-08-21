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
mod sandbox; // OS-level sandbox under approval/cmd_guard: policy · runner · per-platform backends
mod skills; // skill store + registry
mod ui; // tui · theme · markdown · spinner · splash · icons · image_input

mod cli; // one module per `aizen <subcommand>` — the behaviour behind `cli_args`
mod cli_args; // declaration-only: the command-line surface (clap derive types)
mod repl; // the interactive loop's phases: startup · input_pre · turn · postturn · background

// The reorg moved 23 top-level files into the folders above. These re-exports keep the
// call sites in THIS file referring to the modules by their short names (no behavior
// change) — every other file already uses the new `crate::<group>::<mod>` paths.
use crate::agent::prompt_lanes::*;
// Subcommand bodies. Glob-imported so the dispatch in `main` keeps calling them by bare name, and
// so `src/tests/main_suite.rs` (a `#[path]` child of the crate root) still resolves them through
// `use super::*` — the same arrangement the earlier extractions use. `pub(crate)` because these
// names USED to live at the crate root, and `src/ui/menus.rs` reaches them through `use crate::*`;
// re-exporting keeps that reachability byte-identical to before the move.
pub(crate) use crate::cli::{
    agents_cmd::*, apps::*, coop_cmd::*, memory_cmd::*, persona_cmd::*, run_cmds::*, sessions::*,
    skill_cmd::*, time::*, where_report::*,
};
pub(crate) use crate::core::endpoint::*;
use crate::core::session_store::*;
use crate::core::{cli_config, types};
use crate::features::slash_handlers::{
    handle_slash, slash_is_interactive, slash_menu, SlashOutcome,
};
use crate::features::{coop, cron};
use crate::hostbot::platforms::telegram;
use crate::llm::client;
pub(crate) use crate::repl::{background::*, input_pre::*, postturn::*, startup::*, turn::*};
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::context_report::*;
pub(crate) use crate::ui::effort_ui::*;
use crate::ui::menus::{apps_menu, run_discord, run_telegram};
pub(crate) use crate::ui::plain_input::read_input_box;
pub(crate) use crate::ui::provider_ui::*;
use crate::ui::{config_ui, icons, splash, theme, tui};

// The `clap` type tree (every subcommand enum) lives in its own file — see `cli_args`.
use cli_args::*;

use crate::core::approval::ApprovalMode;
use agent::{AgentConfig, AgentOutcome, StopReason};
use anyhow::{Context, Result};
use clap::Parser;
use console::{style, Style};
use dialoguer::theme::ColorfulTheme;
use types::Message;

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
    // `--sandbox` pins the process-wide sandbox mode before anything can spawn a child. User-only:
    // nothing the model outputs can reach this flag (or the env/config it overrides).
    if let Some(mode) = cli.sandbox {
        sandbox::set_mode(mode);
    }
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
                // The daemon is remote autonomous execution: every spawn it makes falls under the
                // sandbox's unattended fail-closed rule (see `sandbox::process_unattended`).
                sandbox::set_process_unattended();
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
        Commands::Sandbox { cmd } => crate::cli::sandbox_cmd::run_sandbox(cmd),
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
            // On a blocking thread on purpose: a `task` dispatch bridges to async with
            // `block_in_place`, and `spawn_blocking` is the context that bridge is verified against
            // (same as the tool executor's parallel path).
            McpCmd::Serve { yes } => {
                tokio::task::spawn_blocking(move || crate::agent::mcp_serve::run(yes)).await?
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

#[cfg(test)]
#[path = "tests/main_suite.rs"]
mod tests;
