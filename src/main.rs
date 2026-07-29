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

// The reorg moved 23 top-level files into the folders above. These re-exports keep the
// call sites in THIS file referring to the modules by their short names (no behavior
// change) — every other file already uses the new `crate::<group>::<mod>` paths.
use crate::agent::app_catalog;
use crate::channels::notify;
use crate::core::{cli_config, config, types};
use crate::features::{commands, crawl, cron, timemachine};
use crate::hostbot::platforms::{discord, telegram};
use crate::llm::client;
use crate::persona::soul;
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::{icons, image_input, splash, theme, tui};

use crate::core::approval::ApprovalMode;
use agent::{AgentConfig, AgentOutcome, StopReason};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use console::{style, Style};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use std::sync::{Mutex, OnceLock};
use types::{Message, ToolDef};

#[derive(Parser, Debug)]
// No explicit `name` — clap uses the package name ("aizen") for `--version` and the actual argv[0]
// (aizen / ng) for the usage string, so each command name prints itself.
#[command(
    version,
    about = "Aizen agentic CLI — streaming chat + a self-learning memory brain"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Stream a chat completion from an OpenAI-compatible endpoint.
    Chat(ChatArgs),
    /// Run the agentic loop: the model uses tools (memory + files + shell) to do a task.
    /// (For the library of specialist sub-agents you can DELEGATE to, see `agents`.)
    Agent(AgentArgs),
    /// Run a workflow: fan out a set of role-scoped sub-agents (from a JSON spec), then
    /// synthesize their results into one answer (mixture-of-agents).
    Workflow(WorkflowArgs),
    /// Manage the CLI's memory brain.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Manage reusable skills (saved step-by-step procedures the agent can load on demand).
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Manage personas — characters the agent role-plays, plus their evolving self-memory.
    Persona {
        #[command(subcommand)]
        cmd: PersonaCmd,
    },
    /// The agent's durable operating-identity (`~/.aizen/SOUL.md` → the `<agent_identity>` slot):
    /// values/policies that hold across EVERY persona and project. With no subcommand, shows it.
    Soul {
        #[command(subcommand)]
        cmd: Option<SoulCmd>,
    },
    /// Run benchmarks.
    Bench {
        #[command(subcommand)]
        cmd: BenchCmd,
    },
    /// Configure the endpoint. With no subcommand, runs an interactive setup (asks for base URL +
    /// key, fetches models, lets you pick one). Or use `set`/`show`/`path`.
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// List the models the provider advertises (GET {base}/models).
    Models(ModelsArgs),
    /// Crawl a website (katana-style): BFS over HTTP, extract links from HTML + endpoints from JS.
    Crawl(CrawlArgs),
    /// Reach doctor: live-probe every web-access backend and show which serves each platform.
    Reach {
        #[command(subcommand)]
        cmd: ReachCmd,
    },
    /// Run the long-lived daemon: listen on Telegram, run the agent on incoming messages, and
    /// route destructive-op approvals to your phone. `--install` registers it as a systemd service
    /// (Linux) so it stays alive across crashes + reboots.
    Serve {
        /// Install as a systemd service (auto-restart + auto-start on boot). Linux only.
        #[arg(long)]
        install: bool,
        /// Remove the systemd service installed by `--install`.
        #[arg(long)]
        uninstall: bool,
        /// Use a per-user systemd unit (`~/.config/systemd/user/`) instead of a system-wide one.
        #[arg(long)]
        user: bool,
        /// With `--install`/`--uninstall`: also run the enable/start (or disable) now, not just print.
        #[arg(long)]
        now: bool,
        /// Paste the bot token and run in one step; owner pairing happens in chat.
        #[arg(long)]
        token: Option<String>,
    },
    /// Configure / test the Telegram bot integration.
    Telegram {
        #[command(subcommand)]
        cmd: TelegramCmd,
    },
    /// Configure / run the Discord bot (two-way): setup · test · serve · show · disable.
    Discord {
        #[command(subcommand)]
        cmd: DiscordCmd,
    },
    /// Time machine — git-backed code snapshots: save · list · restore · undo · redo.
    Time {
        #[command(subcommand)]
        cmd: TimeCmd,
    },
    /// Show where aizen keeps THIS project's state: root, zone slug, git executable, home dirs.
    Where,
    /// Import a conversation recorded by another CLI (Claude Code or Codex) and resume it here.
    /// With no path, lists every foreign transcript whose cwd belongs to this project. With a path,
    /// loads that file directly.
    Import {
        /// Path to a foreign transcript (.jsonl). Omit to list candidates for this project.
        path: Option<String>,
    },
    /// Project zones (the slug keying memory/skills/index): report + merge legacy twins.
    Zone {
        #[command(subcommand)]
        cmd: ZoneCmd,
    },
    /// Schedule agent tasks via the OS scheduler (no daemon): add / list / remove.
    Cron {
        #[command(subcommand)]
        cmd: cron::CronCmd,
    },
    /// MCP (Model Context Protocol) servers from `~/.aizen/mcp.json`: list connected tools.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Connect apps (GitHub, Notion, Slack, Linear, …) via the MCP registry: list · search · add · info · login · remove.
    Apps {
        #[command(subcommand)]
        cmd: Option<AppsCmd>,
    },
    /// Specialist sub-agents you can delegate to (the "agency-agents" library): list · show · where ·
    /// install · remove · enable · disable. Drop-in compatible with `.claude/agents/` markdown.
    #[command(name = "agents")]
    Agents {
        #[command(subcommand)]
        cmd: Option<AgentsCmd>,
    },
    /// Show the installed version alongside every published one and install the one you pick
    /// (newer or older). The running terminal keeps the version it started with; the next terminal
    /// picks up the installed one.
    Update,
    /// Render the moonlit braille art scene (one frame) to the terminal.
    Art,
}

#[derive(Subcommand, Debug)]
enum AppsCmd {
    /// List the featured apps and which are already connected.
    List,
    /// Search the MCP registry for an app/server by keyword.
    Search {
        /// Search keywords.
        query: Vec<String>,
        /// Max results (default 20).
        #[arg(short, long)]
        limit: Option<usize>,
    },
    /// Connect an app: a featured key (github/notion/slack/linear/spotify/google) or a registry name.
    Add {
        /// Featured key or registry server name.
        name: String,
    },
    /// Show a connected app's config (secrets masked) + a live probe of the tools it exposes.
    Info {
        /// The key shown by `aizen apps list`.
        name: String,
    },
    /// Sign in (OAuth) to a connected remote app — opens your browser (Linear/Notion/Slack/Gmail/…).
    Login {
        /// The key shown by `aizen apps list`.
        name: String,
    },
    /// Disconnect an app by its mcp.json key.
    Remove {
        /// The key shown by `aizen apps list`.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    /// List installed specialists (grouped by division; ● pinned to <agents> / ○ not).
    List {
        /// Only this division (e.g. engineering).
        #[arg(short, long)]
        division: Option<String>,
        /// Only this source: aizen-home | claude-home | aizen-project | claude-project.
        #[arg(short, long)]
        source: Option<String>,
        /// Only the agents pinned to the always-on <agents> index.
        #[arg(short, long)]
        enabled: bool,
        /// Machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print one specialist's full card (frontmatter + body).
    Show {
        /// Agent name or slug (from `aizen agents list`).
        name: String,
    },
    /// Show the four source directories and how many agents each holds.
    Where,
    /// Install agents into ~/.aizen/agents from a GitHub repo (owner/repo), a git/.md URL, or a local dir.
    Install {
        /// owner/repo, an https git URL, a single `.md` URL, or a local directory path.
        source: String,
        /// Don't prompt for confirmation before writing.
        #[arg(short, long)]
        yes: bool,
        /// Pin every installed agent to the always-on <agents> index afterwards.
        #[arg(long)]
        enable_all: bool,
        /// For a single `.md` URL: save it under this name (else from frontmatter/URL).
        #[arg(long)]
        as_name: Option<String>,
    },
    /// Remove an installed agent (HOME tree only) and unpin it.
    Remove {
        /// Agent name or slug.
        name: String,
    },
    /// Pin an agent (or --all) to the always-on <agents> index so the model can see it.
    Enable {
        /// Agent name or slug (omit with --all).
        name: Option<String>,
        /// Pin every installed agent.
        #[arg(long)]
        all: bool,
    },
    /// Unpin an agent (or --all) from the always-on <agents> index (still dispatchable by slug).
    Disable {
        /// Agent name or slug (omit with --all).
        name: Option<String>,
        /// Unpin every agent.
        #[arg(long)]
        all: bool,
    },
    /// Pin (or clear) the `model:` a specialist runs on. The model routes through the model→endpoint
    /// registry at dispatch, so it carries its own base_url/api_key (cross-provider sub-agents).
    #[command(name = "set-model")]
    SetModel {
        /// Agent name or slug.
        name: String,
        /// Model id to pin (omit or pass an empty string with --clear to remove the pin).
        model: Option<String>,
        /// Clear the model pin instead of setting one.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand, Debug)]
enum McpCmd {
    /// Connect every enabled server and list its tools (shows the `~/.aizen/mcp.json` path).
    List,
    /// Sign in (OAuth) to a remote server configured with `"auth": "oauth"` — opens your browser.
    Login {
        /// The server's mcp.json key.
        name: String,
    },
    /// Trust THIS repo's project-local `./.aizen/mcp.json` so its servers load (they can run commands).
    Trust,
    /// Stop trusting this repo's project MCP servers.
    Untrust,
}

#[derive(Subcommand, Debug)]
enum SkillCmd {
    /// List saved skills (name — description).
    List {
        /// Also list other workspaces' zones (skills invisible in this project).
        #[arg(long)]
        all_zones: bool,
    },
    /// Print one skill's full steps.
    Show {
        /// Skill name.
        name: String,
    },
    /// Add or overwrite a skill. The body comes from `--body` or stdin.
    Add {
        /// Skill name.
        name: String,
        #[arg(short, long)]
        description: Option<String>,
        #[arg(short, long)]
        when: Option<String>,
        /// The steps/procedure (else read from stdin).
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Delete a skill.
    Delete {
        /// Skill name.
        name: String,
    },
    /// Refine an existing skill's steps in place: archives the prior version, bumps the version,
    /// keeps the usage count. The new body comes from `--body` or stdin.
    Refine {
        /// The existing skill's name.
        name: String,
        /// New one-line summary (kept unchanged if omitted).
        #[arg(short, long)]
        description: Option<String>,
        /// New trigger hint (kept unchanged if omitted).
        #[arg(short, long)]
        when: Option<String>,
        /// The improved steps/procedure (else read from stdin).
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Fetch a skill (markdown, optional frontmatter) from a URL and save it.
    Fetch {
        /// Absolute http(s) URL to a markdown skill file (e.g. a gist/raw GitHub link).
        url: String,
        /// Override the skill name (else taken from frontmatter, else the URL filename).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Search the agentskill.sh marketplace for skills by keyword.
    Search {
        /// Search keywords.
        query: Vec<String>,
        /// Max results (default 20).
        #[arg(short, long)]
        limit: Option<usize>,
    },
    /// Install a skill from agentskill.sh by "owner/name" (or exact name) and save it locally.
    Install {
        /// The skill id, e.g. `NousResearch/spike` (from `aizen skill search`).
        slug: String,
    },
}

#[derive(Subcommand, Debug)]
enum PersonaCmd {
    /// List personas (● = active) with their self-memory counts.
    List,
    /// Print one persona's card.
    Show {
        /// Persona name.
        name: String,
    },
    /// Create or overwrite a persona. The body comes from `--body` or stdin.
    New {
        /// Persona name.
        name: String,
        #[arg(short, long)]
        role: Option<String>,
        #[arg(short, long)]
        voice: Option<String>,
        /// The character body (backstory/values/behavior); else read from stdin.
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Set the active persona (injected as `<persona>` for chat + agent).
    Use {
        /// Persona name.
        name: String,
    },
    /// Clear the active persona (back to the default assistant voice).
    Clear,
    /// Show a character's accumulated self-memory (insights + recent episodes). Defaults to active.
    #[command(name = "self")]
    SelfMem {
        /// Persona name (else the active one).
        name: Option<String>,
    },
    /// Record a free self-memory episode for the ACTIVE persona (no model call).
    Remember {
        /// What the character lived through.
        text: String,
        /// Importance 0–10 (else auto-scored).
        #[arg(short, long)]
        importance: Option<u8>,
    },
    /// Print the assembled `<persona>` + `<self>` blocks the model actually sees.
    Block,
}

#[derive(Subcommand, Debug)]
enum SoulCmd {
    /// Print the operating-identity the model actually sees (sanitized `<agent_identity>` block).
    Show,
    /// Set the operating identity. Body from `--body` or stdin (overwrites any existing SOUL).
    Set {
        /// The identity text (durable values/policies); else read from stdin.
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Remove the operating identity.
    Clear,
    /// Print the SOUL.md file path (edit it directly in any editor).
    Path,
}

#[derive(Subcommand, Debug)]
enum TelegramCmd {
    /// Interactive setup: paste the @BotFather token, then message the bot to capture your chat id.
    Setup,
    /// Send a test message to the configured chat (validates token + chat id).
    Test,
    /// Show the Telegram config (token redacted).
    Show,
}

#[derive(Subcommand, Debug)]
enum ReachCmd {
    /// Live-probe every backend (one tiny request each) and report per-channel health.
    Doctor {
        /// Emit the machine-readable report (the Agent-Reach doctor --json contract).
        #[arg(long)]
        json: bool,
    },
    /// Show the channel table + which backend served each channel this session (no network).
    Status,
}

#[derive(Subcommand, Debug)]
enum TimeCmd {
    /// Save a checkpoint of the current repository's Git-visible tree.
    Save {
        /// Optional label, e.g. `before refactor`.
        label: Vec<String>,
    },
    /// List the timeline (▸ marks the active point).
    List,
    /// Restore the working tree to checkpoint #id (auto-saves the current state first).
    Restore {
        /// Checkpoint id (from `aizen time list`).
        id: u32,
    },
    /// Show what changed between two points in time (checkpoint ids, or `working` for the live tree).
    ///
    /// With one argument, diffs that checkpoint against the working tree — "what have I changed
    /// since #5". With two, diffs the pair. Stat-only by default; `--patch` prints the hunks.
    Diff {
        /// From: a checkpoint id, or `working`. Defaults to the active checkpoint.
        from: Option<String>,
        /// To: a checkpoint id, or `working` (the default).
        to: Option<String>,
        /// Print the unified patch, not just the per-file stat.
        #[arg(short, long)]
        patch: bool,
        /// Limit the diff to these paths.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Emit a machine-readable JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Step one checkpoint back.
    Undo,
    /// Step one checkpoint forward.
    Redo,
    /// Drop oldest checkpoints, keeping at most N (default: the configured limit, 50).
    Prune {
        /// Keep at most this many checkpoints.
        #[arg(short, long)]
        keep: Option<usize>,
    },
    /// Inspect ledger/refs/sidecars/journal without mutating the working tree.
    Doctor {
        /// Emit a machine-readable JSON report.
        #[arg(long)]
        json: bool,
        /// Recover/rollback a valid interrupted transaction before reporting.
        #[arg(long)]
        repair: bool,
    },
    /// Remove orphan Time Machine refs/sidecars after validating the authoritative ledger.
    Gc,
    /// Delete ALL checkpoints (Git objects are reclaimed later by normal Git maintenance).
    Clear,
}

#[derive(Subcommand, Debug)]
enum DiscordCmd {
    /// Interactive setup: paste the bot token + the channel id(s) the bot may respond in.
    Setup,
    /// Validate the bot token (calls /users/@me).
    Test,
    /// Run the bot daemon: listen on Discord, run the agent on incoming messages (Ctrl-C to stop).
    Serve,
    /// Show the Discord bot config (token redacted).
    Show,
    /// Remove the Discord bot config.
    Disable,
}

#[derive(Parser, Debug)]
struct CrawlArgs {
    /// Seed URL(s) to crawl (absolute http(s)). Repeatable.
    #[arg(required = true)]
    urls: Vec<String>,
    /// Max crawl depth (hops from a seed).
    #[arg(short, long, default_value_t = 2)]
    depth: usize,
    /// Hard ceiling on the number of discovered URLs.
    #[arg(long, default_value_t = 200)]
    max_pages: usize,
    /// Scope: `strict` (same host) or `subs` (same root domain + subdomains).
    #[arg(long, default_value = "strict")]
    scope: String,
    /// Concurrent fetches.
    #[arg(short, long, default_value_t = 10)]
    concurrency: usize,
    /// Per-request timeout (seconds).
    #[arg(long, default_value_t = 15)]
    timeout: u64,
    /// Emit JSON ({url, depth, via}) instead of one URL per line.
    #[arg(long)]
    json: bool,
    /// Annotate each URL with its source (seed/html/js) in plain output.
    #[arg(long)]
    show_source: bool,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Set one or more config fields (only the flags you pass are changed).
    Set {
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Context window in tokens for the `% context` HUD (overrides auto-detect/heuristic).
        #[arg(long)]
        context_window: Option<usize>,
        /// Auto-compact threshold as a percent of context (10–95; `0` disables). Default 80.
        #[arg(long)]
        compact_threshold: Option<u8>,
        /// Auto-learn skills from completed multi-step tasks (default on).
        #[arg(long)]
        auto_skill_learn: Option<bool>,
        /// Auto-learn memory: passively learn durable facts from each turn (default on).
        #[arg(long)]
        memory_auto_learn: Option<bool>,
        /// Persona evolution: record episodes + reflect them into insights (default on).
        #[arg(long)]
        persona_evolve: Option<bool>,
        /// `/cost` pricing — USD per 1,000,000 INPUT tokens (enables the $ estimate).
        #[arg(long)]
        price_in: Option<f64>,
        /// `/cost` pricing — USD per 1,000,000 OUTPUT tokens.
        #[arg(long)]
        price_out: Option<f64>,
        /// Icon style: `emoji` (default), `nerd` (needs a Nerd Font), or `off`.
        #[arg(long)]
        icons: Option<String>,
        /// Final-answer visuals: `auto` (when useful), `always` (substantial replies), or `off`.
        #[arg(long)]
        response_visuals: Option<String>,
        /// Time-machine checkpoints to keep (oldest auto-pruned past this; `0` = unlimited). Default 50.
        #[arg(long)]
        timemachine_keep: Option<usize>,
        /// Maximum number of files in one Time Machine snapshot.
        #[arg(long)]
        timemachine_max_files: Option<u64>,
        /// Maximum aggregate bytes in one Time Machine snapshot.
        #[arg(long)]
        timemachine_max_bytes: Option<u64>,
        /// Maximum size of one file in a Time Machine snapshot.
        #[arg(long)]
        timemachine_max_file_bytes: Option<u64>,
        /// Auto-detect reasoning effort per turn from your wording (default on). `false` pins the
        /// fixed --reasoning-effort (or omits it if unset).
        #[arg(long)]
        auto_effort: Option<bool>,
        /// Fixed reasoning effort passed to the provider: `low`, `medium`, `high`, `xhigh`, or `max`.
        /// Setting it turns auto-detect off.
        #[arg(long)]
        reasoning_effort: Option<String>,
        /// Approval level for interactive agent tools: ask, smart, or yolo.
        #[arg(long)]
        approval: Option<String>,
        /// Ultimate mode: pin max reasoning effort + prefer launching workflows (orchestrate-by-default).
        #[arg(long)]
        ultimate: Option<bool>,
        /// Adaptive difficulty→effort routing: let the per-turn heuristic climb to `xhigh` on the
        /// hardest turns (opt-in; default off).
        #[arg(long)]
        adaptive_effort: Option<bool>,
        /// Comma-separated tool bundles to hide.
        #[arg(long)]
        disabled_toolsets: Option<String>,
        /// Comma-separated tool-bundle whitelist.
        #[arg(long)]
        enabled_toolsets: Option<String>,
        /// Default model for dispatched sub-agents (`roles.subagent_default`). Routes through the
        /// model→endpoint registry, so pairing it with `--model-endpoint` runs sub-agents on their
        /// own gateway. Pass an empty string to clear.
        #[arg(long)]
        subagent_model: Option<String>,
        /// Base URL for the sub-agent default endpoint (`roles.subagent_default.base_url`). Usually
        /// unneeded — prefer `--model-endpoint` so the endpoint follows the model. Empty clears.
        #[arg(long)]
        subagent_base_url: Option<String>,
        /// API-key reference for the sub-agent default endpoint: `env:VAR` (preferred) or a literal
        /// key. Empty clears. (`roles.subagent_default.api_key_ref`.)
        #[arg(long)]
        subagent_api_key_ref: Option<String>,
        /// Register a model→endpoint mapping so a sub-agent pinned to that model carries its own
        /// gateway. Format: `model[,base_url=URL][,api_key_ref=env:VAR|KEY]` (repeatable). A bare
        /// model id with no fields, or `model,clear`, removes the entry.
        #[arg(long = "model-endpoint")]
        model_endpoint: Vec<String>,
    },
    /// Show the saved config (API key masked).
    Show,
    /// Print the config file path.
    Path,
}

#[derive(Parser, Debug)]
struct ModelsArgs {
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    api_key: Option<String>,
}

#[derive(Subcommand, Debug)]
enum BenchCmd {
    /// Anti-oracle memory recall bench.
    Memory {
        /// Which query split to run: gate | tune | all.
        #[arg(long, default_value = "gate")]
        split: String,
        /// Capture the current gate metrics as the new baseline.
        #[arg(long)]
        update_baseline: bool,
        /// Also measure the hybrid (lexical + dense) pipeline (uses the pure-Rust hashing
        /// embedder unless built with the `dense` feature).
        #[arg(long)]
        hybrid: bool,
        /// Also measure the fuzzy (Jaro-Winkler bridge) lexical pipeline (W24) — recall/noise
        /// delta vs. the exact-BM25 floor, to decide whether `enable_fuzzy` should default on.
        #[arg(long)]
        fuzzy: bool,
        /// Run the EVOLUTION gate (P8): a multi-session reuse simulation proving recall@5 lifts
        /// ≥5%/session from implicit reinforcement until it plateaus. Standalone (ignores split).
        #[arg(long)]
        evolution: bool,
    },
    /// Golden-set bench for the derived user PROFILE rollup (B2).
    Profile,
    /// Golden-set bench for the DIALECTIC Q&A (B3), incl. abstain-when-unknown.
    Dialectic,
    /// Golden-set bench for the §8 HEALTH metrics: does the store saturate, does recall earn its
    /// budget, do contradictions peak then fall. Reads hand-labeled histories, not the live store.
    Health,
    /// Offline loop-behavior eval (P4): drive the real agent loop with scripted models over ~15
    /// scenarios and report the Section-10 metrics (steps/task, loop-stop rate, verified-done).
    Loop,
}

#[derive(Parser, Debug)]
struct ChatArgs {
    /// One-shot prompt. If omitted, the prompt is read from stdin.
    #[arg(short, long)]
    prompt: Option<String>,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    api_key: Option<String>,
    /// Model id (else AIZEN_MODEL / saved config).
    #[arg(short, long, env = "AIZEN_MODEL")]
    model: Option<String>,
}

#[derive(Parser, Debug)]
struct AgentArgs {
    /// The task for the agent to accomplish.
    task: String,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    api_key: Option<String>,
    /// Model id (else AIZEN_MODEL / saved config).
    #[arg(short, long, env = "AIZEN_MODEL")]
    model: Option<String>,
    /// Pre-authorize destructive tools (file edits / shell) without an interactive prompt.
    #[arg(short, long)]
    yes: bool,
    /// Hard step cap before the one-shot auto-extend (default 25).
    #[arg(long)]
    max_iters: Option<usize>,
}

#[derive(Parser, Debug)]
struct WorkflowArgs {
    /// Path to a workflow spec (JSON): {name, tasks:[{id,role,prompt,model?}], synthesis?:{model?,prompt?}}.
    spec: String,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    api_key: Option<String>,
    /// Default model id for the sub-agents + synthesis (a task's `model` field overrides it for
    /// that task). Else AIZEN_MODEL / saved config.
    #[arg(short, long, env = "AIZEN_MODEL")]
    model: Option<String>,
    /// Pre-authorize destructive tools (file edits / shell) for the sub-agents without prompts.
    #[arg(short, long)]
    yes: bool,
    /// Write a JSON audit trace of the fan-out (per-task model + outcome + synthesis model) here.
    #[arg(long)]
    trace: Option<String>,
}

#[derive(Subcommand, Debug)]
enum MemoryCmd {
    /// Add a new memory (one fact).
    Add {
        /// Short, unique name (becomes the file id).
        name: String,
        /// One-line summary used for ranking/recall.
        #[arg(short, long, default_value = "")]
        description: String,
        /// user | feedback | project | reference (unknown → reference).
        #[arg(short = 't', long = "type", default_value = "reference")]
        mtype: String,
        /// The fact body. If omitted, read from stdin.
        #[arg(short, long)]
        body: Option<String>,
    },
    /// List all memories.
    List {
        /// Workspace view: all (default) | global | current | project | a zone slug.
        #[arg(long)]
        scope: Option<String>,
        /// Show the graveyard instead: facts a correction retired. `revive <id>` brings one back.
        #[arg(long)]
        superseded: bool,
    },
    /// Undo a supersession — put a retired fact back in the live view.
    Revive { id: String },
    /// Show one memory by id or name.
    Show { id: String },
    /// Lexically search memories (long tail; excludes frozen-core entries).
    Search {
        query: String,
        #[arg(short, long, default_value_t = 5)]
        k: usize,
        /// Restrict to one topical dimension: style|tooling|workflow|stack|other.
        #[arg(long)]
        dimension: Option<String>,
        /// Restrict to one content category: bug-history|failed-attempt|success-pattern|arch-decision|command|security-rule|deploy-note|codebase.
        #[arg(long)]
        category: Option<String>,
        /// Workspace view: all (default) | global | current | project | a zone slug.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Show the frozen core (the always-on prompt-prefix block); --rebuild stages a refresh.
    Frozen {
        #[arg(long)]
        rebuild: bool,
    },
    /// Learn from a user turn (free extraction → sanitize → threat-scan → route → store).
    Learn {
        /// The user turn text. If omitted, read from stdin.
        text: Option<String>,
        /// Confirm core (STYLE.md) promotions non-interactively.
        #[arg(short, long)]
        yes: bool,
        /// Classify + report only; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the learned user-style profile (STYLE.md).
    Style,
    /// Show the derived user profile (deterministic preferences rollup, B2).
    Profile {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Ask a natural-language question about the user (B3 dialectic; abstains if unknown).
    Ask {
        /// The question, e.g. "which package manager?" or "should I ask before deleting?".
        question: String,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Inspect the review queue; --promote <id> accepts one, --drop <id> sets one aside,
    /// --clear sets all aside. Discarded items move to `review/.discarded/`, never deleted.
    Review {
        #[arg(long)]
        promote: Option<String>,
        /// Discard a single queued candidate (moved to `review/.discarded/`).
        #[arg(long = "drop")]
        drop_key: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Show what was valid on a given date (bi-temporal history), e.g. 2026-03-01.
    AsOf {
        /// Date in YYYY-MM-DD.
        date: String,
    },
    /// Supersede one memory with another (history kept, not deleted).
    Supersede {
        /// The memory (id or name) that is no longer true.
        old: String,
        /// The memory (id or name) that replaces it.
        new: String,
    },
    /// Edit a stored memory in place (only the flags you pass are changed; the id never changes).
    Edit {
        /// The memory to edit (id or name; a unique prefix works).
        id: String,
        /// New display name.
        #[arg(long)]
        name: Option<String>,
        /// New one-line summary (pass "" to clear it).
        #[arg(short, long)]
        description: Option<String>,
        /// New type: user | feedback | project | reference.
        #[arg(short = 't', long = "type")]
        mtype: Option<String>,
        /// Replace the fact body. If the flag is given with no value, read from stdin.
        #[arg(short, long)]
        body: Option<String>,
        /// Move zones: global | current | project | a zone slug.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Forget a memory: move it to the recoverable archive (undo with `memory restore <id>`).
    Forget {
        /// The memory to forget (id or name; a unique prefix works).
        id: String,
    },
    /// List archived (LRU-evicted) memories.
    Archive,
    /// Restore an archived memory back into the live store (keeps its id).
    Restore {
        id: String,
        /// Restore under a DIFFERENT id. Only needed when the original id is taken — and it breaks
        /// any `supersededBy`/`supersedes` pointer or graph edge that names the old id.
        #[arg(long = "as")]
        as_id: Option<String>,
    },
    /// Permanently delete an ARCHIVED memory (irreversible; `forget` it first).
    Purge {
        /// The archived memory id.
        id: String,
        /// Required — confirms the deletion cannot be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Run anti-bloat maintenance (enforce the inferred-fact LRU cap → archive victims).
    Compact,
    /// Judge suspicious near-duplicate pairs in one model call (dry run unless `--apply`).
    Reconcile {
        /// Actually write the verdicts. Without this the pass only reports what it would do —
        /// the default has to be harmless because the actions it proposes overwrite and retire
        /// facts.
        #[arg(long)]
        apply: bool,
    },
    /// Report what is structurally wrong (or merely invisible) in the store. Read-only.
    Doctor,
    /// Show the three §8 health metrics per week of use (saturation, recall usefulness,
    /// contradictions found). Read-only; reads `stats.jsonl` + the learning audit.
    Health,
    /// Show a fact's strongest co-retrieval associations (the Hebbian graph, P5).
    Neighbors {
        /// The memory (id or name) whose neighbors to list.
        id: String,
        /// Max neighbors to show.
        #[arg(short, long, default_value_t = 10)]
        k: usize,
    },
    /// Download the dense-tier embedding model into `~/.aizen/models/<name>/` (one-time, P6).
    /// Fetches the three files the dense backend loads locally; pair with a `--features dense`
    /// build to actually use them. Re-running only fetches files not already present.
    ModelDownload {
        /// Model name (a HF `minishlab/<name>` repo). Defaults to the configured embed model.
        #[arg(long)]
        name: Option<String>,
    },
    /// Show every model2vec model already on this machine and which one the dense tier would pick.
    ///
    /// The dense tier used to look at ONE path (`~/.aizen/models/<configured name>`) and fall back
    /// to the non-semantic hashing embedder if that exact dir was absent — even with a perfectly
    /// usable model sitting next to it. This lists what discovery actually finds, so "why is dense
    /// not using my model?" is answerable without a source dive.
    ModelList,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Restore the terminal (leave alt screen, show cursor, reset scroll region + cooked stdin) BEFORE
    // the default panic printer runs, so a panic inside retained/sticky mode never dumps its backtrace
    // into the alternate screen or onto a frame with a restricted scroll region. Idempotent.
    crate::ui::tui::install_panic_hook();
    let cli = Cli::parse();
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
        Commands::Config { cmd } => run_config(cmd).await,
        Commands::Models(args) => run_models(args).await,
        Commands::Crawl(args) => run_crawl(args).await,
        Commands::Reach { cmd } => run_reach(cmd).await,
        Commands::Serve {
            install,
            uninstall,
            user,
            now,
            token,
        } => {
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
                hostbot::run_serve().await
            }
        }
        Commands::Telegram { cmd } => run_telegram(cmd).await,
        Commands::Discord { cmd } => run_discord(cmd).await,
        Commands::Time { cmd } => run_time(cmd),
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
        Commands::Art => {
            crate::ui::moonscape::run();
            Ok(())
        }
    }
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

// ───────────────────────────── telegram daemon + setup ─────────────────────────────

const SERVE_HELP: &str = "Aizen is listening. Send a message to chat with the agent \
(read-only tools; destructive ops will ask you to approve here). Follow-ups keep context, so \
\"now fix it\" works. Prefix with `/agent ` to run fully autonomously (file edits / shell without \
asking). /new (or /reset) starts a fresh conversation · /resume shows how much context is kept · \
/help shows this.";

/// Discord has no inline approval routing yet (unlike Telegram's ✓/✗ buttons), so destructive ops
/// are auto-DENIED on a plain message — the agent simply skips them. This help text says so honestly
/// instead of promising an approval prompt that never arrives. Use `/agent ` to run autonomously.
const DISCORD_HELP: &str = "Aizen is listening. Send a message to chat with the agent \
(read-only tools work as-is). Discord can't show approval prompts yet, so file edits / shell are \
SKIPPED unless you prefix with `/agent ` to run fully autonomously (no approval needed). \
Follow-ups keep context, so \"now fix it\" works. /new (or /reset) starts a fresh conversation · \
/help shows this.";

/// Split text under a platform's UTF-16 limit, preferring newline boundaries so table records and
/// text-diagram rows stay intact. A single over-limit line falls back to scalar-safe hard splitting.
fn chunk_text(s: &str, max: usize) -> Vec<String> {
    if s.encode_utf16().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_units = 0usize;
    for segment in s.split_inclusive('\n') {
        let units = segment.encode_utf16().count();
        if units <= max {
            if cur_units + units > max && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_units = 0;
            }
            cur.push_str(segment);
            cur_units += units;
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        let mut piece = String::new();
        let mut piece_units = 0usize;
        for ch in segment.chars() {
            let u = ch.len_utf16();
            if piece_units + u > max && !piece.is_empty() {
                out.push(std::mem::take(&mut piece));
                piece_units = 0;
            }
            piece.push(ch);
            piece_units += u;
        }
        cur = piece;
        cur_units = piece_units;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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

/// Hard cap on messages retained in one serve (Telegram) session, so a long conversation can't grow
/// without bound. Generous — the mid-loop context guard handles within-turn pressure.
const SERVE_SESSION_MAX_MSGS: usize = 40;

/// Bound a serve session's history: drop the OLDEST whole turns (keeping the system prompt at [0])
/// until under `max`, always cutting at a `user` boundary so an assistant tool-call turn is never
/// split from its tool results (a dangling tool_call ⇒ a 400 on strict gateways).
fn cap_session(history: &mut Vec<Message>, max: usize) {
    let lead = agent::compact::leading_system_count(history).max(1);
    while history.len() > max {
        // index of the SECOND user message (the start of the 2nd turn); drop after the system prefix.
        let second_user = history
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= lead && m.role == "user")
            .nth(1)
            .map(|(i, _)| i);
        match second_user {
            Some(i) if i > lead => {
                history.drain(lead..i);
            }
            _ => break, // only one turn present → nothing safe to drop; the loop guard handles it
        }
    }
}

/// Run one `aizen serve` turn over a PERSISTENT per-chat history, so follow-ups like "now fix it" keep
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
        // Built once per session → the stable lane stays byte-stable across the conversation.
        let bundle = refreshed_system_prompt_bundle(model);
        history.push(Message::system(bundle.stable));
        if !bundle.dynamic.trim().is_empty() {
            history.push(Message::system(bundle.dynamic));
        }
    }
    history.push(Message::user(task.to_string()));

    arm_lsp_session();
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        resolve_ctx_window(model).0,
    )?;
    let cfg = AgentConfig {
        approval_mode,
        quiet: true,
        enable_verify_gate: false,
        context_window: resolve_ctx_window(model).0,
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        ..Default::default()
    };
    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        client::chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
    };
    // Mid-loop auto-compaction for long serve sessions: a NON-streaming summarize closure over the
    // same endpoint. `cap_session` below stays only as a hard backstop (compaction usually keeps the
    // history well under its cap).
    let sum_ep = summarizer_endpoint(base, key, model_ref);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    let outcome =
        agent::run_agent_loop_compacting(chat, summarize, &cfg, &registry, history).await?;

    // The bot path keeps the FREE regex learning and does NOT run the end-of-turn secretary. Two
    // reasons, both worth stating rather than leaving as an accident of the refactor:
    //   - a bot has no cwd, so `place` anchoring is meaningless here; it reads `user`/`device` facts
    //     through the frozen core, which is machine-stable after phase 1.
    //   - the secretary is a model call per gated turn. On a chat bot that is a standing cost the
    //     operator should opt into, not inherit.
    // Because no REPL loop calls this any more, the two write paths can no longer both fire on one
    // turn — which was the reason to worry about having both.
    maybe_learn_memory(history);
    cap_session(history, SERVE_SESSION_MAX_MSGS);

    if let StopReason::AwaitingInput(q) = &outcome.stop {
        return Ok(format!("❓ {q}"));
    }
    Ok(outcome
        .final_text
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(the agent produced no answer)".to_string()))
}

/// `aizen serve` — the long-lived daemon: one poll loop owns getUpdates, an agent runner handles one
/// message at a time, and destructive-op approvals route to the phone (via the approval gate).
async fn run_serve() -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (client, cfg) = telegram::configured()
        .context("telegram not configured — run `aizen telegram setup` first")?;
    let (base_url, api_key, model) = resolve_endpoint(None, None, None)
        .context("configure the model endpoint first (run `aizen config`)")?;
    let http = http_client()?;

    telegram::set_daemon_active(true);
    let client = Arc::new(client);
    eprintln!(
        "{}",
        style(format!(
            "aizen serve — listening on Telegram (Ctrl-C to stop). chats: {:?}",
            cfg.allowed_chat_ids
        ))
        .dim()
    );

    let (tx, mut rx) = mpsc::channel::<(i64, String)>(64);

    let poll_client = client.clone();
    let poll_cfg = cfg.clone();
    let poll = tokio::spawn(async move {
        let mut offset = 0i64;
        loop {
            let updates = match poll_client
                .get_updates(offset, telegram::POLL_TIMEOUT_SECS)
                .await
            {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("[poll] {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
            };
            for u in updates {
                offset = offset.max(u.update_id + 1);
                if let Some(cb) = u.callback_query {
                    let chat = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
                    if telegram::is_allowed(&poll_cfg, chat) {
                        if let Some((id, ok)) =
                            cb.data.as_deref().and_then(telegram::parse_callback)
                        {
                            telegram::resolve_approval(&id, ok);
                        }
                    }
                    let _ = poll_client.answer_callback(&cb.id, "").await;
                    continue;
                }
                if let Some(msg) = u.message {
                    if telegram::is_allowed(&poll_cfg, msg.chat.id) {
                        if let Some(text) = msg.text {
                            let _ = tx.send((msg.chat.id, text)).await;
                        }
                    }
                }
            }
        }
    });

    // Per-chat conversation history → follow-ups ("now fix it") keep context. In-memory only
    // (a daemon restart starts fresh); `/new`/`/reset` clear a chat, `/resume` reports its size.
    let mut sessions: std::collections::HashMap<i64, Vec<Message>> =
        std::collections::HashMap::new();

    loop {
        let (chat, text) = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => { eprintln!("\nshutting down…"); crate::agent::process::kill_all(); break; }
            m = rx.recv() => match m { Some(m) => m, None => break },
        };
        let trimmed = text.trim();
        if trimmed == "/help" || trimmed == "/start" {
            let _ = client.send_message(chat, SERVE_HELP).await;
            continue;
        }
        if trimmed == "/new" || trimmed == "/reset" {
            sessions.remove(&chat);
            let _ = client
                .send_message(
                    chat,
                    "🆕 started a fresh conversation — earlier context dropped.",
                )
                .await;
            continue;
        }
        if trimmed == "/resume" {
            let turns = sessions
                .get(&chat)
                .map(|h| h.iter().filter(|m| m.role == "user").count())
                .unwrap_or(0);
            let msg = if turns == 0 {
                "🧵 no active conversation — just send a message to start one.".to_string()
            } else {
                format!("🧵 continuing — {turns} message(s) of context kept. /new to start over.")
            };
            let _ = client.send_message(chat, &msg).await;
            continue;
        }
        let (task, approval) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), ApprovalMode::Yolo),
            None => (trimmed.to_string(), approval_mode()),
        };
        if task.is_empty() {
            continue;
        }
        let _ = client.send_message(chat, "⏳ working…").await;
        let history = sessions.entry(chat).or_default();
        let reply = run_serve_turn(&http, &base_url, &api_key, &model, history, &task, approval)
            .await
            .unwrap_or_else(|e| format!("error: {e}"));
        let shown = crate::ui::markdown::render_plain_blocks(&reply);
        for piece in chunk_text(&shown, 3500) {
            let _ = client.send_message(chat, &piece).await;
        }
    }

    telegram::set_daemon_active(false);
    poll.abort();
    Ok(())
}

// ───────────────────────── project identity (where + zones) ─────────────────────────

#[derive(Subcommand, Debug)]
enum ZoneCmd {
    /// Find artifacts stored under LEGACY slugs of this project (the pre-2026-07 remote-URL /
    /// verbatim-path keying) and merge them into the current zone. Dry-run by default: it only
    /// REPORTS. `--apply` executes; every action is printed; clashes are moved aside, never
    /// overwritten — with ONE exception: the codebase-index cache keeps the newer of two copies
    /// and drops the other (`/init` rebuilds it). If you keep several checkouts of this repo, a
    /// URL-keyed legacy zone is shared between them — migrating claims it for THIS checkout.
    Migrate {
        /// Execute the merge (without this flag: report only).
        #[arg(long)]
        apply: bool,
    },
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
    if let Some(l) = crate::features::zones::quick_legacy_probe() {
        let _ = writeln!(
            s,
            "⚠ legacy zone : {l} — data from the old slug keying; `aizen zone migrate` shows what would merge (--apply to do it)"
        );
    }
    s.trim_end().to_string()
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
        TimeCmd::Gc => {
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
mod rel_time_tests {
    use super::*;

    fn at(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn buckets_seconds_minutes_hours_days() {
        let now = at("2026-07-08 12:00:00");
        assert_eq!(rel_time_from("2026-07-08 11:59:30", now), "30s ago");
        assert_eq!(rel_time_from("2026-07-08 11:58:00", now), "2m ago");
        assert_eq!(rel_time_from("2026-07-08 09:00:00", now), "3h ago");
        assert_eq!(rel_time_from("2026-07-05 12:00:00", now), "3d ago");
    }

    #[test]
    fn future_and_now_and_malformed() {
        let now = at("2026-07-08 12:00:00");
        // A clock-skewed future timestamp degrades to "just now", not a negative age.
        assert_eq!(rel_time_from("2026-07-08 12:00:30", now), "just now");
        assert_eq!(rel_time_from("2026-07-08 12:00:00", now), "0s ago");
        // Unparseable input falls back to the raw string.
        assert_eq!(rel_time_from("not-a-date", now), "not-a-date");
    }
}

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

/// `aizen discord serve` — the Discord bot daemon. A gateway task receives messages (heartbeating
/// independently); this loop runs the agent one message at a time (per-channel history) and replies
/// over REST. Mirrors `run_serve` (Telegram). NOTE: destructive-op approvals are not yet routed to
/// Discord, so edits need `/yolo`/smart approval; read/research work as-is.
async fn run_discord_serve() -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (client, cfg) =
        discord::configured().context("Discord bot not configured — run `aizen discord setup`")?;
    let (base_url, api_key, model) = resolve_endpoint(None, None, None)
        .context("configure the model endpoint first (run `aizen config`)")?;
    let http = http_client()?;
    let token = cfg.resolved_token().context("no bot token")?;
    let client = Arc::new(client);
    eprintln!(
        "{}",
        style(format!(
            "aizen serve — listening on Discord (Ctrl-C to stop). channels: {:?}",
            cfg.allowed_channel_ids
        ))
        .dim()
    );

    let (tx, mut rx) = mpsc::channel::<discord::Incoming>(64);
    let gw_cfg = cfg.clone();
    let gw = tokio::spawn(async move { discord::run_gateway(token, gw_cfg, tx).await });

    // Per-channel conversation history → follow-ups keep context (in-memory; /new resets).
    let mut sessions: std::collections::HashMap<u64, Vec<Message>> =
        std::collections::HashMap::new();
    loop {
        let inc = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => { eprintln!("\nshutting down…"); crate::agent::process::kill_all(); break; }
            m = rx.recv() => match m { Some(m) => m, None => break },
        };
        let trimmed = inc.content.trim();
        if trimmed == "/help" || trimmed == "/start" {
            let _ = client.send_message(inc.channel_id, DISCORD_HELP).await;
            continue;
        }
        if trimmed == "/new" || trimmed == "/reset" {
            sessions.remove(&inc.channel_id);
            let _ = client
                .send_message(
                    inc.channel_id,
                    "🆕 started a fresh conversation — earlier context dropped.",
                )
                .await;
            continue;
        }
        let (task, approval) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), ApprovalMode::Yolo),
            None => (trimmed.to_string(), approval_mode()),
        };
        if task.is_empty() {
            continue;
        }
        let _ = client.send_message(inc.channel_id, "⏳ working…").await;
        let history = sessions.entry(inc.channel_id).or_default();
        let reply = run_serve_turn(&http, &base_url, &api_key, &model, history, &task, approval)
            .await
            .unwrap_or_else(|e| format!("error: {e}"));
        let shown = crate::ui::markdown::render_plain_blocks(&reply);
        for piece in chunk_text(&shown, discord::MESSAGE_MAX) {
            let _ = client.send_message(inc.channel_id, &piece).await;
        }
    }
    gw.abort();
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

/// Pick a skill to delete (Esc cancels).
fn skill_delete_interactive(skills: &[skill::Skill]) {
    let theme = ui_theme();
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    if let Ok(Some(i)) = Select::with_theme(&theme)
        .with_prompt("Delete which skill? (Esc to cancel)")
        .items(&names)
        .default(0)
        .interact_opt()
    {
        match skill::delete(&names[i]) {
            Ok(true) => println!(
                "{}",
                style(format!("deleted '{}'", names[i])).color256(splash::ACCENT)
            ),
            Ok(false) => println!("{}", style("(already gone)").dim()),
            Err(e) => eprintln!("{} {e}", style("skill:").red()),
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
        let mut items: Vec<String> = skills
            .iter()
            .map(|s| {
                let d = if s.description.is_empty() {
                    s.when.clone()
                } else {
                    s.description.clone()
                };
                format!("{}  —  {}", s.name, style(d).dim())
            })
            .collect();
        items.push("+ New skill".to_string());
        items.push("⬇ Fetch from URL".to_string());
        items.push(format!(
            "🔎 Search agentskill.sh  {}",
            style("(marketplace)").dim()
        ));
        if n > 0 {
            items.push("✗ Delete a skill".to_string());
        }
        items.push("Back".to_string());
        let prompt = format!("Skills — {n} saved (Esc to go back)");
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
                eprintln!("{} {e}", style("skill:").red());
            }
        } else if pick == n + 1 {
            if let Err(e) = skill_fetch_interactive().await {
                eprintln!("{} {e}", style("skill:").red());
            }
        } else if pick == n + 2 {
            if let Err(e) = skill_search_interactive().await {
                eprintln!("{} {e}", style("skill:").red());
            }
        } else if n > 0 && pick == n + 3 {
            skill_delete_interactive(&skills);
        } else {
            return Ok(()); // Back
        }
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
    let resp = client::chat_with_tools(&http, &base_url, &api_key, &model_id, &[sys, usr], &[])
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
        for m in insights.iter().take(10) {
            println!(
                "  {} [{}] {}",
                style("★").color256(splash::ACCENT),
                m.importance,
                truncate_chars(m.body.trim(), 140)
            );
        }
    }
    let episodes: Vec<&persona::self_mem::SelfMemory> = mems
        .iter()
        .filter(|m| m.kind == persona::self_mem::Kind::Episode)
        .collect();
    if !episodes.is_empty() {
        println!("\n{}", style("recent episodes").dim());
        for m in episodes.iter().take(8) {
            println!(
                "  {} [{}] {}",
                style("·").dim(),
                m.importance,
                truncate_chars(m.body.trim(), 120)
            );
        }
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
                    eprintln!("{} {e}", style("persona:").red());
                }
            }
            "Paste a character prompt → auto-create" => {
                if let Err(e) = persona_paste_interactive(history, model).await {
                    eprintln!("{} {e}", style("persona:").red());
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
                    .with_prompt("Delete which persona? (Esc to cancel)")
                    .items(&names)
                    .default(0)
                    .interact_opt()
                {
                    match persona::delete(&names[i]) {
                        Ok(true) => println!(
                            "{}",
                            style(format!("deleted '{}'", names[i])).color256(splash::ACCENT)
                        ),
                        Ok(false) => println!("{}", style("(already gone)").dim()),
                        Err(e) => eprintln!("{} {e}", style("persona:").red()),
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
        3 => hostbot::run_serve().await,
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
fn ui_theme() -> ColorfulTheme {
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

    if let Err(e) = config_wizard().await {
        // A cancelled/failed wizard shouldn't abort the launch — fall through into the menu.
        eprintln!("{} {e}", style("setup:").color256(crate::ui::theme::WARN));
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
            eprintln!("{} {e}", style("apps:").color256(crate::ui::theme::WARN));
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

/// Disarms the interactive cancel token however a turn ends — normal completion, an early `continue`
/// from a prep failure, or a panic unwinding out of the arm. `disarm_cancel` is identity-checked, so
/// this can never clear a NEWER turn's token and double-disarming is harmless.
///
/// This exists because the token is now armed BEFORE the turn's prep work (see the Chat arm), and an
/// armed token is what `tui::turn_in_flight` reports. Leaking one past a `continue` would leave the
/// REPL idle while Esc still behaved like "cancel", so every exit path must disarm.
struct TurnCancelGuard(crate::core::cancel::TurnCancel);

impl Drop for TurnCancelGuard {
    fn drop(&mut self) {
        tui::disarm_cancel(&self.0);
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

/// The sticky-TUI REPL: a background keyboard thread feeds a submission queue while the agent runs,
/// the input box stays pinned at the bottom, and Esc/Ctrl-C cancels an in-flight turn.
async fn run_menu_sticky() -> Result<()> {
    let http = http_client()?;
    let mut model_label = cli_config::load()
        .model
        .unwrap_or_else(|| "(no model)".to_string());
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
    } else if let Some((name, n, origin)) = most_recent_session() {
        // OFFER the previous conversation instead of waiting to be asked. Every turn was already
        // autosaved, but nothing on this screen said so — a reopened terminal looked like a blank
        // slate, so the transcript sat on disk unmentioned and the user retyped their context.
        // Suppressed when a crash-recovery offer is showing: two competing restore prompts in a row
        // is worse than one, and `/recover` (which carries an unsent draft + checkpoint id) wins.
        // A session from ANOTHER project is only offered when this one has none — labeled, so
        // resuming it is a visible choice.
        let origin_note = origin.map(|o| format!(", {o}")).unwrap_or_default();
        tui::emit_line(
            &style(format!(
                "⟲ last conversation “{}” ({n} messages{origin_note}) — /resume to continue it",
                pretty_session_name(&name)
            ))
            .dim()
            .to_string(),
        );
    }
    // Background model health poller: colours the idle `● ready` chip green/yellow/red from a real
    // GET /models probe every 60s (plus once immediately). Independent of the chat HTTP client so a
    // long-running turn's keep-alive doesn't share the short health timeout.
    spawn_health_poller();
    spawn_reconcile_pass();
    let mut input = tui::spawn_input();

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
                if !line.is_empty() {
                    let (cleaned, file_imgs) = image_input::extract_image_attachments(&line);
                    if !file_imgs.is_empty() {
                        images.extend(file_imgs);
                        line = cleaned;
                    }
                }
                if line.is_empty() && images.is_empty() {
                    continue;
                }
                // ARM CANCEL FIRST — before any prep. Everything between here and `set_working(true)`
                // is real latency the user can see and will try to interrupt: `@file` expansion, the
                // dynamic prompt-lane rebuild, codebase retrieval, the recovery checkpoint, LSP
                // spawn, registry construction. The token being armed is what makes `turn_in_flight`
                // true, so Esc cancels throughout that window instead of silently clearing the draft
                // while the turn starts anyway. The guard disarms on EVERY exit path (including the
                // `continue`s below), so an aborted prep never leaves Esc mis-wired.
                let turn_cancel = crate::core::cancel::TurnCancel::new();
                tui::arm_cancel(turn_cancel.clone());
                let _cancel_guard = TurnCancelGuard(turn_cancel.clone());
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
                migrate_legacy_prompt_lanes(&mut history, &model);
                refresh_dynamic_prompt_lane(&mut history, &model);
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
                let sent = fold_context_into_query(&line);
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
                // Refresh the exit-flush snapshot the moment the turn's user message lands, so an
                // abrupt window close mid-turn still persists the question (per-turn autosave only
                // runs on success).
                update_live_history(&history);
                let persona_before = cli_config::load().persona;
                // Arm LSP BEFORE building the registry — tools only register while enabled.
                arm_lsp_session();
                let registry = match agent::builtin::default_registry_with_task(
                    http.clone(),
                    base_url.clone(),
                    api_key.clone(),
                    model.clone(),
                    approval_mode(),
                    resolve_ctx_window(&model).0,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        history.pop();
                        continue;
                    }
                };
                let cfg = AgentConfig {
                    approval_mode: approval_mode(),
                    cancel: turn_cancel.clone(),
                    context_window: resolve_ctx_window(&model).0,
                    enable_self_review: cli_config::self_review_enabled(&cli_config::load()),
                    // Reflect the live manager state (honors `/lsp off` for this turn).
                    enable_lsp: crate::agent::lsp::LSP.is_enabled(),
                    // Goal mode (set by `/goal <text>`): threads the live goal into this turn so the
                    // loop runs cap-free with smart retry until the goal is declared + verified.
                    goal: crate::agent::goal::current_goal(),
                    // Only the interactive top-level turn reads the steering mailbox — a course
                    // correction the user typed is aimed at THIS task, not at whatever a delegated
                    // sub-agent happens to be doing (children keep the `false` default).
                    enable_steering: true,
                    // Keep the exit-flush snapshot current DURING the turn, not just at its edges.
                    on_progress: Some(publish_live_history),
                    ..AgentConfig::default()
                };

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
                // Open the steering mailbox for this turn: Alt+Enter now hands a message to the RUNNING
                // loop (folded in at its next step) instead of the post-turn queue.
                crate::core::steer::arm();
                while input.cancel.try_recv().is_ok() {} // drain any stale wake-up
                                                         // A quiet "here we go" line: the whimsical working verb ("✦ Pondering…") prints ONCE
                                                         // into the scrolling transcript at turn start, so each run opens on a fresh word —
                                                         // instead of the verb cycling in the cramped HUD pill. This path only runs under the
                                                         // sticky TUI (already a TTY); silenced with tips off (`AIZEN_NO_TIPS`).
                if !cli_config::branded_flag("NO_TIPS") {
                    tui::emit_line(
                        &style(format!("✦ {}…", tui::next_work_verb()))
                            .color256(splash::ACCENT)
                            .to_string(),
                    );
                }
                // Arm LAST: the keyboard thread only queues a cancel once WORKING is true, so flipping
                // it after the clear+drain guarantees no Esc meant for THIS turn gets swallowed in the
                // arming window.
                tui::set_working(true);
                crate::core::recovery::set_phase(
                    crate::core::recovery::RecoveryPhase::WaitingModel,
                );

                // Run the turn racing a cancel signal; on cancel the future is DROPPED at its current
                // await (model stream / tool batch / verify gate), which aborts the in-flight request.
                // History stays consistent under the drop because the loop PRE-FILLS: the assistant
                // tool-call turn and one placeholder result per call are appended in a single
                // synchronous block before any tool await, and real results overwrite the
                // placeholders as they land (see agent::execute_calls).
                let result = {
                    let http_ref = &http;
                    let base = base_url.as_str();
                    let key = api_key.as_str();
                    let model_ref = model.as_str();
                    let registry_ref = &registry;
                    let cfg_ref = &cfg;
                    let eager_on = eager_enabled();
                    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
                        if eager_on {
                            // Read-only calls start the moment their streamed args complete.
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
                            client::stream_chat_with_tools(
                                http_ref, base, key, model_ref, &msgs, &defs,
                            )
                            .await
                        }
                    };
                    // Non-streaming summarizer for mid-loop auto-compaction (keeps the streamed display clean).
                    let sum_ep = summarizer_endpoint(base, key, model_ref);
                    let summarize = move |msgs: Vec<Message>| {
                        let ep = sum_ep.clone();
                        async move {
                            client::chat_with_tools(
                                http_ref,
                                &ep.base_url,
                                &ep.api_key,
                                &ep.model,
                                &msgs,
                                &[],
                            )
                            .await
                            .map(|t| t.content.unwrap_or_default())
                        }
                    };
                    // Optional oracle for self-review: only when `roles.oracle` is explicitly
                    // configured (a stronger reviewer model); otherwise nudge-mode.
                    let oracle = cli_config::role_configured("oracle")
                        .then(|| {
                            cli_config::resolve_role(
                                "oracle",
                                &cli_config::ResolvedEndpoint {
                                    base_url: base.to_string(),
                                    api_key: key.to_string(),
                                    model: model_ref.to_string(),
                                },
                            )
                        })
                        .map(|ep| {
                            move |msgs: Vec<Message>| {
                                let ep = ep.clone();
                                async move {
                                    client::chat_with_tools(
                                        http_ref,
                                        &ep.base_url,
                                        &ep.api_key,
                                        &ep.model,
                                        &msgs,
                                        &[],
                                    )
                                    .await
                                    .map(|t| t.content.unwrap_or_default())
                                }
                            }
                        });
                    let fut = agent::run_agent_loop_full(
                        chat,
                        summarize,
                        oracle,
                        &cfg,
                        &registry,
                        &mut history,
                    );
                    tokio::select! {
                        r = fut => Some(r),
                        // Match only a REAL signal: if the keyboard thread exits (read_key error/EOF)
                        // its cancel_tx drops and recv() resolves to None — the `Some(())` pattern
                        // fails, tokio disables this branch, and the turn completes instead of being
                        // spuriously killed with "(interrupted by user)".
                        Some(()) = input.cancel.recv() => None,
                    }
                };
                tui::set_working(false);
                tui::disarm_cancel(&turn_cancel);
                // Close the steering mailbox. Anything typed in the last instants of the turn (after
                // the loop's final drain) comes back here rather than vanishing — re-inject it as an
                // ordinary submission so it runs as the next turn. On Esc the `None` arm below flushes
                // the queue, which is the right call there: stop means stop.
                for leftover in crate::core::steer::disarm() {
                    let _ = input
                        .inject
                        .send(tui::Submission::Chat(leftover, Vec::new()));
                    tui::note_submission_enqueued();
                }
                crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::Finalizing);
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
                        surface_abnormal_stop(&outcome);
                        // Goal mode finishes only on a verify-passing `Done` (the goal gate lets the
                        // turn reach Done solely after `goal_complete` + a green verify gate). Clear it
                        // here so the next turn is an ordinary capped turn again. Esc (Cancelled) lands
                        // in the `None` arm and intentionally leaves the goal armed — the user can retry.
                        if crate::agent::goal::current_goal().is_some()
                            && matches!(outcome.stop, StopReason::Done)
                        {
                            crate::agent::goal::set_goal(None);
                            crate::agent::goal::arm(false);
                            crate::agent::goal::clear();
                            tui::emit_line(
                                &style("🎯 goal complete — verified. goal mode off.")
                                    .color256(splash::ACCENT)
                                    .to_string(),
                            );
                        }
                        // An EMPTY answer from a SINGLE model call (no tool work, no streamed text)
                        // used to vanish silently — a blank turn (a rate-limit swallowed into an empty
                        // 200, a content filter, a dead endpoint that streams `[DONE]` with no deltas)
                        // looked identical to "still idle". Surface it so a failed/empty call never
                        // passes for success. Gated on `iters <= 1` so a turn that DID do tool work and
                        // simply ended without a closing sentence isn't wrongly flagged.
                        let empty = outcome
                            .final_text
                            .as_deref()
                            .map(str::trim)
                            .unwrap_or("")
                            .is_empty();
                        if empty && outcome.iters <= 1 {
                            // A blank turn is a FAILURE, not idle — surface it loudly (warn colour, not
                            // a dim aside) so an empty 200 / content filter / rate-limit-swallowed-as-200
                            // / dead endpoint that streams `[DONE]` with no deltas can never pass silently
                            // for success. This is the "don't swallow API errors" contract.
                            tui::emit_line(&format!(
                                "{} the model returned an empty response — no text and no tool calls. Likely a rate limit, content filter, or a gateway that closed the stream early. Try again, or /model to switch.",
                                theme::warn("⚠ empty reply:")
                            ));
                        }
                        let persona_after = cli_config::load().persona;
                        if persona_after != persona_before {
                            update_system_prompt(&mut history, &model);
                            if let Some(name) = persona_after {
                                tui::emit_line(
                                    &style(format!(
                                        "🎭 now playing: {name} (from your next message)"
                                    ))
                                    .color256(splash::ACCENT)
                                    .to_string(),
                                );
                            }
                        }
                        // The post-turn passes are model calls (the secretary, persona reflection,
                        // auto-compaction) and they run here, after `set_working(false)` and after
                        // the turn's token was disarmed. So the pill was down, `turn_in_flight()`
                        // was false, and Esc took the idle branch — while the REPL sat awaiting them and
                        // consumed no input. To the user the turn had visibly ENDED and the app was
                        // wedged anyway. Re-arm for the duration; cancelling skips the remaining
                        // learning, which is always optional work.
                        let learned = cancellable_slash(async {
                            maybe_run_secretary(&history, &http, &base_url, &api_key, &model).await;
                            maybe_evolve_persona(&http, &base_url, &api_key, &model).await;
                            maybe_auto_compact(&mut history, &http, &base_url, &api_key, &model)
                                .await;
                        })
                        .await;
                        if learned.is_none() {
                            tui::emit_line(
                                &theme::muted("⏹ skipped the post-turn learning passes.")
                                    .to_string(),
                            );
                        }
                        // Persistence is NOT optional, so it sits outside that block: a cancelled
                        // learning pass must still leave the conversation on disk. `autosave_session`
                        // names the session with a model call, so it's cancellable too — falling back
                        // to the local-only writer keeps the transcript either way.
                        if cancellable_slash(autosave_session(
                            &history, &http, &base_url, &api_key, &model,
                        ))
                        .await
                        .is_none()
                        {
                            autosave_last(&history, Some(&model));
                        }
                    }
                    Some(Err(e)) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        if history.last().map(|m| m.role == "user").unwrap_or(false) {
                            history.pop();
                        }
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
    let mut history: Vec<Message> = Vec::new();
    let mut input_history: Vec<String> = Vec::new(); // recallable past prompts (↑/↓ in the box)
    rebuild_system(&mut history, &model_label);
    install_exit_flush_handler(); // flush the live chat if the terminal window is closed (Windows ✕)
    if let Some((name, n, origin)) = most_recent_session() {
        let origin_note = origin.map(|o| format!(", {o}")).unwrap_or_default();
        println!(
            "{}",
            style(format!(
                "⟲ last conversation “{}” ({n} messages{origin_note}) — /resume to continue it",
                pretty_session_name(&name)
            ))
            .dim()
        );
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
        if !line.is_empty() {
            let (cleaned, file_imgs) = image_input::extract_image_attachments(&line);
            if !file_imgs.is_empty() {
                images.extend(file_imgs);
                line = cleaned;
            }
        }
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
            if let Some(rest) = line.strip_prefix('/') {
                // Bare `/` (+ Enter) → arrow-key command picker; `/cmd` runs directly.
                let outcome = if rest.trim().is_empty() {
                    slash_menu(&mut history, &mut model_label).await
                } else {
                    handle_slash(rest, &mut history, &mut model_label).await
                };
                match outcome {
                    SlashOutcome::Quit => break,
                    // A custom command expanded to a prompt → run it as a chat turn (not re-preprocessed).
                    SlashOutcome::Submit(prompt) => line = prompt,
                    SlashOutcome::Continue => continue,
                }
            } else {
                typed_src = line.clone();
                match preprocess_input(&line) {
                    InputPre::Handled => continue, // #remember / !shell-escape — no turn
                    InputPre::Send(expanded) => line = expanded,
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
        migrate_legacy_prompt_lanes(&mut history, &model);
        refresh_dynamic_prompt_lane(&mut history, &model);
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
        let sent = fold_context_into_query(&line);
        if images.is_empty() {
            history.push(Message::user(sent));
        } else {
            println!(
                "{}",
                style(format!(
                    "📎 {} image{} attached",
                    images.len(),
                    if images.len() == 1 { "" } else { "s" }
                ))
                .color256(splash::ACCENT)
            );
            history.push(Message::user_with_images(sent, images));
        }
        // Refresh the exit-flush snapshot the moment the user turn lands — so a window close mid-turn
        // (before the per-turn autosave) still persists this message.
        update_live_history(&history);
        // Snapshot the active persona so we can detect an in-turn switch (the `persona_create` tool)
        // and resync the system prompt at the turn boundary — prefix-cache safe, takes effect next msg.
        let persona_before = cli_config::load().persona;
        arm_lsp_session();
        let registry = match agent::builtin::default_registry_with_task(
            http.clone(),
            base_url.clone(),
            api_key.clone(),
            model.clone(),
            approval_mode(),
            resolve_ctx_window(&model).0,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {e}", style("error:").red());
                history.pop();
                continue;
            }
        };
        // Unified ask/smart/yolo approval, with AIZEN_YES forcing yolo.
        let turn_cancel = crate::core::cancel::TurnCancel::new();
        let cfg = AgentConfig {
            approval_mode: approval_mode(),
            cancel: turn_cancel,
            context_window: resolve_ctx_window(&model).0,
            enable_self_review: cli_config::self_review_enabled(&cli_config::load()),
            enable_lsp: crate::agent::lsp::LSP.is_enabled(),
            // Goal mode (set by `/goal <text>`): threads the live goal so the loop runs cap-free
            // with smart retry until the goal is declared + verified.
            goal: crate::agent::goal::current_goal(),
            // Same mid-turn snapshot as the sticky REPL: an abrupt close mid-turn keeps the work
            // done so far instead of only the question.
            on_progress: Some(publish_live_history),
            ..AgentConfig::default()
        };
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
        let sum_ep = summarizer_endpoint(base, key, model_ref);
        let summarize = move |msgs: Vec<Message>| {
            let ep = sum_ep.clone();
            async move {
                client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                    .await
                    .map(|t| t.content.unwrap_or_default())
            }
        };
        let oracle = cli_config::role_configured("oracle")
            .then(|| {
                cli_config::resolve_role(
                    "oracle",
                    &cli_config::ResolvedEndpoint {
                        base_url: base.to_string(),
                        api_key: key.to_string(),
                        model: model_ref.to_string(),
                    },
                )
            })
            .map(|ep| {
                move |msgs: Vec<Message>| {
                    let ep = ep.clone();
                    async move {
                        client::chat_with_tools(
                            http_ref,
                            &ep.base_url,
                            &ep.api_key,
                            &ep.model,
                            &msgs,
                            &[],
                        )
                        .await
                        .map(|t| t.content.unwrap_or_default())
                    }
                }
            });
        match agent::run_agent_loop_full(chat, summarize, oracle, &cfg, &registry, &mut history)
            .await
        {
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
                // Mirror of the sticky path: a non-`Done` stop is not success and must not render as
                // one. See `surface_abnormal_stop`.
                surface_abnormal_stop(&outcome);
                // Surface an empty single-call turn (empty 200 / content filter / gateway that
                // closed the stream early) as a visible warning instead of a silent no-op — the
                // plain-REPL mirror of the sticky path's `⚠ empty reply` line.
                let empty = outcome
                    .final_text
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
                if empty && outcome.iters <= 1 {
                    eprintln!(
                        "{} the model returned an empty response — no text and no tool calls. Likely a rate limit, content filter, or a gateway that closed the stream early. Try again, or /model to switch.",
                        style("⚠ empty reply:").yellow()
                    );
                }
                // The agent may have created/switched personas mid-turn (persona_create tool).
                // Resync the system prompt now so the new character is live from the next message.
                let persona_after = cli_config::load().persona;
                if persona_after != persona_before {
                    update_system_prompt(&mut history, &model);
                    if let Some(name) = persona_after {
                        println!(
                            "{}",
                            style(format!("🎭 now playing: {name} (from your next message)"))
                                .color256(splash::ACCENT)
                        );
                    }
                }
                // File what the turn was worth, from the full detail BEFORE compaction summarizes
                // it away. ONE call for facts + episode + skill — this loop used to run skill,
                // persona and memory in a different order than the retained loop above.
                maybe_run_secretary(&history, &http, &base_url, &api_key, &model).await;
                // Periodic: distill accumulated episodes into durable character insights.
                maybe_evolve_persona(&http, &base_url, &api_key, &model).await;
                maybe_auto_compact(&mut history, &http, &base_url, &api_key, &model).await;
                // Auto-checkpoint so /sessions can always restore where you left off (no manual save).
                autosave_session(&history, &http, &base_url, &api_key, &model).await;
            }
            Err(e) => {
                eprintln!("{} {e}", style("error:").red());
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
    let resp = match client::chat_with_tools(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
        .await
    {
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
    if let Some(sk) = out.skill.as_ref() {
        if auto_skill_learn_enabled() {
            let slug = skill::sanitize_name(&sk.name);
            let exists = skill::list()
                .iter()
                .any(|s| skill::sanitize_name(&s.name) == slug);
            let done = if exists {
                // Only fold when the model MEANT to; otherwise a same-named skill is a collision to
                // leave alone, not a licence to overwrite the user's procedure.
                sk.refine && skill::refine(&sk.name, &sk.steps, None, Some(&sk.when)).is_ok()
            } else {
                skill::save_scoped(&sk.name, "", &sk.when, &sk.steps, true).is_ok()
            };
            if done {
                tui::emit_line(
                    &style(format!(
                        "{}{} skill '{}' — /skills to view",
                        icons::g(icons::learned()),
                        if exists { "refined" } else { "learned" },
                        sk.name
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
    let resp = match client::chat_with_tools(
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
        if persona::self_mem::save_insight(slug, &ins.text, ins.importance).is_ok() {
            saved += 1;
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
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
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

/// Flattened active prompt for callers that still expect a single string.
fn current_system_prompt(model: &str) -> String {
    active_system_prompt_bundle(model).flatten()
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
fn refresh_prompt_lanes_for_thread_switch(history: &mut Vec<Message>, model: &str) {
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
    if let Some((block, pairs)) = memory::recall_block(query, MEMORY_RECALL_BUDGET_TOKENS) {
        memory::pending::open_turn(pairs);
        out = format!("{block}\n\n{out}");
    }
    out
}

/// Drop recall blocks from user turns already in `history`.
///
/// Each block was true for the turn it rode in on. Left in place they accumulate — ten turns of
/// standing facts re-stated ten times — and, worse, an older block can contradict a newer one with
/// nothing in the transcript marking which came later, so the model has to guess.
///
/// Called only from [`maybe_auto_compact`], at the moment the prefix cache is being invalidated
/// anyway: rewriting a user turn at any other time would break cache coverage for the whole tail,
/// costing more than the tokens it saves.
///
/// Matches on [`memory::RECALL_MARKER`] at the start of the content and cuts through the first blank
/// line. Anything the user actually typed survives, including a message that merely mentions the
/// phrase — the marker has to be at position 0, which only our own folding produces.
fn strip_recall_blocks(history: &mut [Message]) {
    for m in history.iter_mut() {
        if m.role != "user" {
            continue;
        }
        let Some(content) = m.content.as_deref() else {
            continue;
        };
        let stripped = memory::strip_recall_prefix(content);
        if stripped.len() != content.len() {
            m.content = Some(stripped.to_string());
        }
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
fn effective_ctx_window(model: &str, configured: Option<usize>) -> (usize, bool) {
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
mod context_breakdown_tests {
    use super::*;

    #[test]
    fn splits_system_prompt_into_blocks() {
        let sys = "BASE RULES HERE\n\n<environment>\ncwd: /x\n</environment>\n\
                   <user_memory>\n- terse\n</user_memory>\n<skills>\nidx\n</skills>\n";
        let rows = system_block_chars(sys);
        // base instructions is always first.
        assert_eq!(rows[0].0, "base instructions");
        let labels: Vec<&str> = rows.iter().map(|(l, _)| *l).collect();
        assert!(labels.contains(&"environment"));
        assert!(labels.contains(&"user memory"));
        assert!(labels.contains(&"skills index"));
        // absent blocks aren't reported.
        assert!(!labels.contains(&"persona"));
        assert!(!labels.contains(&"agents index"));
        // block char counts + base sum to the whole prompt (nothing double-counted or dropped).
        let sum: usize = rows.iter().map(|(_, c)| *c).sum();
        assert_eq!(sum, sys.chars().count());
    }

    #[test]
    fn base_only_prompt_reports_just_base() {
        let sys = "just the base, no tagged blocks";
        let rows = system_block_chars(sys);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("base instructions", sys.chars().count()));
    }

    #[test]
    fn fold_retrieval_passthrough_when_empty_or_no_index() {
        // Empty / whitespace query → returned verbatim (nothing to retrieve against).
        assert_eq!(fold_retrieval_into_query(""), "");
        assert_eq!(fold_retrieval_into_query("   "), "   ");
        // A real query is either an identity passthrough (no index) or `block\n\n{query}` (index
        // hit). Either way the ORIGINAL query text is preserved intact at the END — the RAG fold
        // only ever PREPENDS attributed context, never rewrites the user's words. (Robust whether or
        // not this repo has a persisted /init index, since tests share the process cwd.)
        let q = "how does the payment flow work";
        let sent = fold_retrieval_into_query(q);
        assert!(
            sent == q || sent.ends_with(&format!("\n\n{q}")),
            "query must be preserved: {sent:?}"
        );
    }

    #[test]
    fn context_fold_preserves_the_user_text_and_never_touches_system_lanes() {
        // Invariant I1: both per-turn blocks ride on the USER turn, so system lanes 0/1 stay
        // byte-stable and the provider's prefix cache keeps covering the transcript tail.
        let q = "does the user prefer pnpm";
        let sent = fold_context_into_query(q);
        assert!(
            sent == q || sent.ends_with(&format!("\n\n{q}")),
            "the user's own words must survive verbatim at the end: {sent:?}"
        );

        // The fold is a pure string transform over the query — it takes no `history`, so there is no
        // path by which it could write a system lane. Assert the shape that makes that true: any
        // injected block is a PREFIX, never a rewrite of the tail.
        let mut history = vec![
            Message::system("stable lane"),
            Message::system("dynamic lane"),
        ];
        let before: Vec<Option<String>> = history.iter().map(|m| m.content.clone()).collect();
        history.push(Message::user(sent));
        strip_recall_blocks(&mut history);
        let after: Vec<Option<String>> =
            history.iter().take(2).map(|m| m.content.clone()).collect();
        assert_eq!(
            before, after,
            "system lanes must be untouched byte-for-byte"
        );
    }

    #[test]
    fn strip_recall_blocks_drops_our_block_but_keeps_what_the_user_typed() {
        let typed = "why is the build slow";
        let folded = format!(
            "{} (may be stale…):\n[m1] (about you) prefers pnpm\n\n{typed}",
            memory::RECALL_MARKER
        );
        let mut history = vec![
            Message::system("lane"),
            Message::user(folded),
            Message::assistant("some reply"),
            // A user message that merely MENTIONS the phrase mid-sentence must be left alone: only
            // our own folding puts the marker at position 0.
            Message::user(format!("tell me about {} handling", memory::RECALL_MARKER)),
        ];
        strip_recall_blocks(&mut history);

        assert_eq!(
            history[1].content.as_deref(),
            Some(typed),
            "block stripped, question kept"
        );
        assert_eq!(
            history[2].content.as_deref(),
            Some("some reply"),
            "assistant turns untouched"
        );
        assert!(
            history[3]
                .content
                .as_deref()
                .is_some_and(|c| c.contains("handling")),
            "a user message that only mentions the marker must not be truncated"
        );

        // Idempotent: compacting twice must not eat the real message.
        strip_recall_blocks(&mut history);
        assert_eq!(history[1].content.as_deref(), Some(typed));
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

/// Truncate to `max` chars with a `…[+N chars]` marker (delegates to the shared compaction core).
fn truncate_chars(s: &str, max: usize) -> String {
    agent::compact::truncate_chars(s, max)
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
            client::chat_with_tools(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
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
    let summary =
        client::chat_with_tools(&http, &ep.base_url, &ep.api_key, &ep.model, &prompt, &[])
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

const SLASH_HELP: &str = "\
Commands:
  /help              this list
  /init [--force|--status]  index the codebase into a semantic chunk index (SHA-256 incremental, secrets redacted); powers codebase_search + auto per-turn retrieval. --force rebuilds, --status shows state, Esc cancels
  /where             show THIS project's identity: root · zone slug · git executable · where memory/skills/sessions live (also `aizen where`, `aizen zone migrate`)
  /model             list the provider's models (with context windows) + pick one
  /config            set endpoint + key + model (wizard)
  /memory [query]    show your profile, or search memory; /memory remember <fact> to save
  /persona           pick the character the agent role-plays (list · select · new · clear · delete)
  /skills            saved procedures the agent can load (list · view · new · delete)
  /commands          your custom slash commands — markdown macros in ~/.aizen/commands/ ($ARGUMENTS · @file · !`cmd`)
  /apps              connected apps & MCP catalog — Telegram/Discord/Slack/webhook + browser sign-in apps
  /mcp               MCP servers from ~/.aizen/mcp.json — lifecycle generation, health, pinned schema + tools
  /browser           browser profile/routes status (when built with --features browser)
  /telegram          Telegram integration menu (setup · test · status · start daemon · disable)
  /sessions          saved conversations — restore · save · delete (autosaves into its own file each turn; newest first, labeled by project)
  /resume [name]     reopen the last conversation FROM THIS PROJECT (or a named one); /handoff <goal> starts a fresh thread carrying only what that goal needs
  /import            resume a conversation started in another CLI (Claude Code or Codex) — pick from transcripts whose cwd matches this project
  /where             which project/zone you're in, and which file this conversation is saved to
  /workflows         multi-agent status — live task/workflow children, sub-agent slots (also /wf)
  /agents            specialist sub-agents you can delegate to — list · set-model <name> <model> (routes model→endpoint)
  /recover           a session interrupted by a crash/kill — restore its transcript + unsent draft, or /recover discard
  /timemachine       browse every checkpoint (▸ = current) and pick one to jump back to that code + chat; also /timeline · /tm · /undo · /redo
  /diff              what changed in the working tree since a checkpoint (read before you /undo)
  /checkpoint [note] save a restore point of the working tree now
  /compact           summarize older turns to free context now
  /goal <text>       run until the goal is done — model self-declares (goal_complete) + verify passes; no iteration cap, auto-retries API errors (incl. empty 200); /goal off to stop, Esc to cancel
  /lsp [on|off|status|restart]  type-aware navigation + symbol_replace/insert + diagnostics via a language server (rust-analyzer · pyright · typescript-language-server); default ON (lazy spawn), /lsp off reclaims RAM
  /reach [doctor|status]  web-access channels: live-probe every backend (doctor) or show what served this session (status); web_fetch/web_search route through these
  /approval [ask|smart|yolo]  approval level — ask every time, auto-run read-only, or pre-authorize
  /ultimate          toggle ultimate mode — max reasoning effort + prefer launching workflows (aizen's ultracode)
  /effort            drag an animated slider (auto · low · medium · high · xhigh · max); or /effort auto|off|low|medium|high|xhigh|max|clear to set it directly
  /update            show the installed version next to every published one and install the one you pick (newer or older) — the new build starts in your NEXT terminal
  /clear             start a fresh conversation
  /tokens            show session token usage (context-fill HUD)
  /context           break down what fills the context window (system prompt · tool schemas · conversation by role)
  /cost              session usage + $ estimate (real tokens when the provider reports them; set rates via `aizen config set --price-in/--price-out`)
  /quit              exit

Input shortcuts (in a normal message):
  #<text>            remember <text> as a durable fact (one keystroke into the memory brain) — sends no turn
  !<cmd>             run <cmd> in the shell and show output (the safety floor still blocks catastrophic commands) — sends no turn
  @<path>            inline a file's contents into your message
  !`<cmd>`           splice a read-only command's output into your message
Anything else you type goes to the agent (it chats and uses tools in one loop).";

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

async fn slash_workflows(_arg: &str) {
    let status = agent::orchestration::format_status();
    if !tui::retained_overlay_open("Activity", &status) {
        tui::emit_line(&status);
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
fn fmt_time_ago(built_unix: u64) -> String {
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

/// `/agents` — list installed specialists with their model pin; `/agents set-model <name> <model>`
/// pins (or, with no model / `clear`, clears) the model a specialist runs on. The pin routes through
/// the model→endpoint registry at dispatch, so it carries its own gateway (cross-provider).
fn slash_agents(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
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
            for def in &all {
                let slug = def.slug();
                let pin = enabled.as_ref().map(|s| s.contains(&slug)).unwrap_or(true);
                let mark = if pin { "●" } else { "○" };
                let model = def.model.as_deref().unwrap_or("(parent model)");
                out.push_str(&format!("  {mark} {:<24} model: {model}\n", slug));
            }
            out.push_str("\nset a model:  /agents set-model <name> <model>   ·   clear:  /agents set-model <name> clear");
            tui::emit_line(&out.trim_end().to_string());
        }
        other => {
            tui::emit_line(&style(format!("unknown /agents subcommand '{other}' — try /agents or /agents set-model <name> <model>")).dim().to_string());
        }
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
    match name {
        "help" | "?" | "" => tui::emit_line(&style(SLASH_HELP).dim().to_string()),
        "quit" | "exit" | "q" => return SlashOutcome::Quit,
        "clear" | "new" | "reset" => {
            rebuild_system(history, model_label);
            reset_per_session_state(); // fresh todos/cost/grants/@refs for the new conversation
            set_session_slug(None); // the next turn names + autosaves a brand-new session file
            update_live_history(history); // drop the old chat from the exit-flush snapshot too, so an
                                          // immediate window-close after /clear doesn't re-save it
            tui::emit_line(&style("(new conversation)").dim().to_string());
        }
        "where" => {
            tui::emit_line(&where_report());
            // In the REPL, "where" includes WHICH FILE this conversation is being written to.
            let sess = match current_session_slug() {
                Some(s) => format!("session:  {}", sessions_dir().join(format!("{s}.json")).display()),
                None => "session:  (not saved yet — named on the first autosave)".to_string(),
            };
            tui::emit_line(&style(sess).dim().to_string());
        }
        "tokens" => print_status_line(history, model_label),
        "context" | "ctx" => print_context(history, model_label),
        "cost" | "usage" => print_cost(history, model_label),
        // /save + /load folded into /sessions (the current chat autosaves under its own name).
        "save" | "load" => {
            tui::emit_line(&style("→ use /sessions — restore / save / delete are all there now").dim().to_string());
        }
        "sessions" => {
            if let Err(e) = sessions_menu(history, model_label).await {
                eprintln!("{} {e}", style("sessions:").red());
            }
        }
        "import" => {
            if let Err(e) = import_menu(history, model_label).await {
                eprintln!("{} {e}", style("import:").red());
            }
        }
        // One keystroke back into the last conversation. `/sessions` could already restore, but it
        // costs a menu and knowing which of a dozen files is the newest — so in practice a reopened
        // terminal started from scratch even though the transcript was on disk the whole time.
        "resume" | "continue" => {
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
        "workflows" | "workflow" | "wf" | "agents-status" => slash_workflows(arg).await,
        "agents" | "agent" => slash_agents(arg),
        "recover" | "recovery" => {
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
        "compact" => {
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
        "handoff" => {
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
        "goal" => {
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
        "lsp" => {
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
        "reach" => {
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
        "update" => {
            // Talks to GitHub, so wrap in `cancellable_slash` for Esc; owns stdin through the
            // dialoguer picker, which is why `tui::slash_takes_stdin` suspends the frame for it.
            match cancellable_slash(features::update::run()).await {
                None => tui::emit_line(&style("update cancelled").dim().to_string()),
                Some(Err(e)) => tui::emit_line(&format!("{} {e:#}", style("update:").red())),
                Some(Ok(())) => {}
            }
        }
        "approval" => {
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
        "yolo" | "auto" | "yes" => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Yolo { ApprovalMode::Ask } else { ApprovalMode::Yolo };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /yolo alias)")).color256(splash::ACCENT).to_string());
        }
        "smart" => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Smart { ApprovalMode::Ask } else { ApprovalMode::Smart };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /smart alias)")).color256(splash::ACCENT).to_string());
        }
        "ultimate" | "ultra" => {
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
            if std::env::var("AIZEN_ULTIMATE").is_ok() {
                tui::emit_line(&style("(note: AIZEN_ULTIMATE is set in your environment — it forces ultimate ON regardless of this toggle)").dim().to_string());
            }
        }
        "effort" => {
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
        "model" | "models" => {
            if let Err(e) = slash_model(model_label).await {
                if tui::active() {
                    tui::emit_line(&format!("{} {e}", style("model:").red()));
                } else {
                    eprintln!("{} {e}", style("model:").red());
                }
            } else {
                // Also in place: the help text promises `/model` "switches models mid-session", and a
                // switch that silently discarded the session would make that promise a lie. The stable
                // lane carries `model:`, so it must be rewritten — see `refresh_prompt_lanes_in_place`.
                refresh_prompt_lanes_in_place(history, model_label);
            }
        }
        "config" | "setup" => {
            if let Err(e) = config_wizard().await {
                eprintln!("{} {e}", style("config:").red());
            }
            *model_label = cli_config::load().model.unwrap_or_else(|| model_label.clone());
            // Refresh IN PLACE — retuning settings mid-chat must not end the conversation.
            refresh_prompt_lanes_in_place(history, model_label);
        }
        "memory" | "mem" => {
            if let Err(e) = slash_memory(arg) {
                eprintln!("{} {e}", style("memory:").red());
            }
        }
        "persona" | "personas" | "character" => {
            if let Err(e) = personas_menu(history, model_label).await {
                eprintln!("{} {e}", style("persona:").red());
            }
        }
        "skills" | "skill" => {
            if let Err(e) = skills_menu().await {
                eprintln!("{} {e}", style("skills:").red());
            }
        }
        "apps" | "integrations" => {
            if let Err(e) = apps_menu().await {
                eprintln!("{} {e}", style("apps:").red());
            }
        }
        "mcp" => tui::emit_line(&crate::agent::mcp::summary()),
        "browser" => {
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
        "tools" | "toolsets" => slash_tools(arg).await,
        "commands" | "cmds" => match commands::summary() {
            Some(s) => tui::emit_line(&style(s).dim().to_string()),
            None => tui::emit_line(
                &style("No custom commands yet. Drop a markdown file in ~/.aizen/commands/ (or ./.aizen/commands/ for this project) — see /help.").dim().to_string()
            ),
        },
        "telegram" | "tg" => {
            if let Err(e) = telegram_menu().await {
                eprintln!("{} {e}", style("telegram:").red());
            }
        }
        // `/serve` kept as a direct shortcut to the daemon (also reachable via the Telegram menu).
        "serve" => {
            if let Err(e) = hostbot::run_serve().await {
                eprintln!("{} {e}", style("serve:").red());
            }
        }
        // ── time machine (git snapshots) ──
        // `/timemachine` is ONE command: it opens the checkpoint list, and picking a row rewinds to
        // that code + chat. No `pick`/`restore` argument to remember, no separate read-only print —
        // the list itself shows the history, so browsing and restoring are the same gesture (Esc
        // leaves without touching anything).
        "timemachine" | "timeline" | "tm" => {
            if let Err(e) = timemachine_menu(history, model_label).await {
                eprintln!("{} {e}", style("time:").red());
            }
        }
        // Capture the conversation alongside the tree so a pick in `/timemachine` can rewind chat as
        // well as code — a `/checkpoint` is a deliberate save point where the chat is worth keeping,
        // unlike the loop's per-edit auto-snapshots (which restore files only).
        "checkpoint" | "snapshot" | "cp" => match timemachine::save_with_chat(arg, false, history) {
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
        "diff" | "changes" => {
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
        "undo" => match timemachine::undo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏪ rewound to").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("undo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        "redo" => match timemachine::redo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏩ re-applied").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("redo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // Codebase index: scan the repo into a semantic chunk index for `codebase_search` +
        // per-turn retrieval injection. `/init` incrementally refreshes; `/init --force` rebuilds
        // from scratch; `/init --status` shows the current index without scanning. Esc cancels.
        "init" | "index" => {
            slash_init(arg).await;
        }
        // A user-defined command (`~/.aizen/commands/<name>.md`) → expand its template and run it
        // as a normal chat turn. Falls back to "unknown" only when no command matches.
        other => match commands::find(other) {
            Some(cmd) => match commands::expand(&cmd, arg) {
                Ok(prompt) if !prompt.trim().is_empty() => return SlashOutcome::Submit(prompt),
                Ok(_) => tui::emit_line(&style(format!("/{other} expanded to an empty prompt")).dim().to_string()),
                Err(e) => tui::emit_line(&format!("{} {e}", style(format!("/{other}:")).red())),
            },
            None => tui::emit_line(&style(format!("unknown command /{other} — try /help")).dim().to_string()),
        },
    }
    SlashOutcome::Continue
}

/// Checkpoint helpers — persist / restore the REPL conversation under ~/.aizen/sessions/.
fn sessions_dir() -> std::path::PathBuf {
    config::aizen_home().join("sessions")
}
fn sanitize_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let n = n.trim_matches(['.', ' ']);
    // Windows device names resolve specially even with an extension (`CON.json`, `NUL.json`, …).
    // Prefix them so a saved/restored session can never target a device path.
    let upper = n.to_ascii_uppercase();
    let numbered_device = |prefix: &str| {
        upper
            .strip_prefix(prefix)
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0")
    };
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered_device("COM")
        || numbered_device("LPT");
    if n.is_empty() {
        "session".to_string()
    } else if reserved {
        format!("session_{n}")
    } else {
        n.to_string()
    }
}
/// Why a save-as name can't be used, or `None` if it can. Split out of the picker's arm so the rule
/// is testable without driving the interactive prompt.
///
/// Only `last` is refused, and only here: it is the retired legacy-pointer name, which
/// [`scan_sessions`] deliberately skips. Accepting it would print "saved" for a file the picker can
/// neither restore nor delete, and pin every later autosave to it. Not folded into
/// [`sanitize_name`], which must keep mapping `last` verbatim so the legacy pointer stays loadable
/// and re-homable.
fn session_save_name_error(raw: &str) -> Option<&'static str> {
    (sanitize_name(raw.trim()) == "last").then_some(
        "“last” is the retired pointer name — pick another (it would not show up in /sessions)",
    )
}

/// Suggest a human-readable session name from the conversation's first user turn, so the "Save as"
/// prompt comes PRE-FILLED with the topic (Enter to accept, or edit) instead of a blank box. A short
/// hyphenated slug of the first few meaningful words + a date suffix to keep same-topic saves distinct.
fn suggest_session_name(history: &[Message]) -> String {
    let date = chrono::Local::now().format("%m%d").to_string();
    let first = history
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    // Skip slash-command / leading noise; take the first line, lowercase, keep word-ish chars.
    let line = first
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let words: Vec<String> = line
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| w.len() >= 2)
        .take(5)
        .collect();
    let slug = words.join("-");
    // Cap length so long first messages don't produce an unwieldy default.
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("chat-{date}")
    } else {
        format!("{slug}-{date}")
    }
}
/// Provenance stamped into every saved session so a file can answer "which project, which model,
/// when?" without the user cross-referencing anything. Every field is optional: a hand-edited or
/// pre-provenance file still parses, absent just means "unknown".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SessionMeta {
    /// Normalized canonical path key of the project root — the exact string `project_slug()`
    /// hashes, so "same project?" agrees byte-for-byte with zone identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

/// On-disk shape of a saved session: `{"version":2,"meta":{…},"messages":[…]}`. The
/// pre-provenance format was a bare `Vec<Message>` array — [`parse_session_bytes`] accepts both.
/// `version` and `meta` both default: a future writer may bump the version, and a hand-written or
/// partially-written file that still has `messages` is worth loading. Only a missing/unparsable
/// `messages` makes a file unreadable.
#[derive(serde::Deserialize)]
struct SessionFile {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    meta: SessionMeta,
    messages: Vec<Message>,
}

/// Borrowed twin of [`SessionFile`] for writing, so every autosave doesn't clone the transcript.
#[derive(serde::Serialize)]
struct SessionFileRef<'a> {
    version: u32,
    meta: &'a SessionMeta,
    messages: &'a [Message],
}

/// Parse either session format. `None` = unreadable/corrupt (callers surface that explicitly —
/// a corrupt file must never masquerade as an empty conversation).
fn parse_session_bytes(bytes: &[u8]) -> Option<(Vec<Message>, Option<SessionMeta>)> {
    if let Ok(f) = serde_json::from_slice::<SessionFile>(bytes) {
        return Some((f.messages, Some(f.meta)));
    }
    serde_json::from_slice::<Vec<Message>>(bytes)
        .ok()
        .map(|m| (m, None))
}

fn save_session(history: &[Message], name: &str, model: Option<&str>) -> Result<String> {
    // All three come from one cached identity lookup, so a file's key and slug can never disagree.
    write_session(
        history,
        name,
        SessionMeta {
            project_key: Some(config::project_key()),
            project_root: Some(config::project_root().display().to_string()),
            project_slug: Some(config::project_slug()),
            model: model
                .map(str::to_string)
                .or_else(|| cli_config::load().model),
            created: None,
            updated: None,
        },
    )
}

/// Re-home an UNATTRIBUTED transcript — the legacy `last.json` pointer copy — under a real slug
/// without inventing provenance for it. The pointer never recorded which project it came from, so
/// stamping the current one would assert on disk that another repo's conversation belongs here:
/// that lie then silences [`load_session`]'s cross-project warning, makes the file read as `here`
/// forever, and can never be undone (there is no original path left to restore). Absent fields are
/// the truth. Whatever the source DID record is carried through verbatim.
fn rehome_session(
    history: &[Message],
    name: &str,
    carried: Option<SessionMeta>,
    model: Option<&str>,
) -> Result<String> {
    let mut meta = carried.unwrap_or_default();
    meta.model = model.map(str::to_string).or(meta.model);
    write_session(history, name, meta)
}

/// How many saved sessions still carry a LEGACY zone slug — for `aizen zone migrate`'s plan.
/// Sessions are a flat pool keyed by the provenance INSIDE each file, so they are invisible to the
/// slug-directory sweep the rest of the migration does.
pub(crate) fn count_sessions_of_slug(legacy_slug: &str) -> usize {
    stat_sessions()
        .iter()
        .filter(|s| {
            read_session_row(&s.path)
                .1
                .and_then(|m| m.project_slug)
                .is_some_and(|sl| sl == legacy_slug)
        })
        .count()
}

/// Re-stamp every session recorded under `legacy_slug` with the CURRENT project identity — the
/// session leg of `aizen zone migrate`. Without it, a moved/re-cloned checkout (or a pre-fix
/// twin-zone population) left every one of the user's OWN transcripts reading as another project:
/// labeled `from <old dir>` in the picker and warned about on restore, permanently, because nothing
/// else in the migration touches provenance stored inside files.
///
/// `updated` is preserved: a bookkeeping rewrite must not make a stale conversation look freshly
/// used, exactly as memory retagging preserves its aging clock. (The file's mtime does move — std
/// has no portable way to set it, and adding a C dependency for cosmetics isn't worth it — so
/// `updated` stays the honest record of when the conversation itself last changed.)
pub(crate) fn retag_sessions_of_slug(legacy_slug: &str, on_error: &mut dyn FnMut(String)) -> usize {
    let key = config::project_key();
    let root = config::project_root().display().to_string();
    let slug = config::project_slug();
    let mut n = 0usize;
    for s in stat_sessions() {
        let Some((msgs, Some(mut meta))) = std::fs::read(&s.path)
            .ok()
            .and_then(|b| parse_session_bytes(&b))
        else {
            continue;
        };
        if meta.project_slug.as_deref() != Some(legacy_slug) {
            continue;
        }
        meta.project_key = Some(key.clone());
        meta.project_root = Some(root.clone());
        meta.project_slug = Some(slug.clone());
        let file = SessionFileRef {
            version: 2,
            meta: &meta,
            messages: &msgs,
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(mut b) => {
                b.push(b'\n');
                b
            }
            Err(e) => {
                on_error(format!("session {}: {e:#}", s.name));
                continue;
            }
        };
        match crate::core::persist::atomic_write(&s.path, &bytes)
            .and_then(|_| crate::core::persist::harden_owner_only_checked(&s.path))
        {
            Ok(_) => n += 1,
            Err(e) => on_error(format!("session {}: {e:#}", s.name)),
        }
    }
    n
}

/// Write a session file. `created` is preserved across the per-turn re-saves of one conversation
/// (existing file's stamp wins, then the caller's carried one, then now); `updated` is always now.
fn write_session(history: &[Message], name: &str, mut meta: SessionMeta) -> Result<String> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    config::harden_dir(&dir);
    let path = dir.join(format!("{}.json", sanitize_name(name)));
    let existing_created = std::fs::read(&path)
        .ok()
        .and_then(|b| parse_session_bytes(&b))
        .and_then(|(_, m)| m.and_then(|m| m.created));
    let now = chrono::Local::now().to_rfc3339();
    meta.created = existing_created
        .or(meta.created)
        .or_else(|| Some(now.clone()));
    meta.updated = Some(now);
    let file = SessionFileRef {
        version: 2,
        meta: &meta,
        messages: history,
    };
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    // The transcript can contain pasted secrets / .env contents → owner-only.
    crate::core::persist::harden_owner_only_checked(&path)?;
    Ok(path.display().to_string())
}
fn load_session(history: &mut Vec<Message>, name: &str, model: &str) -> Result<usize> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    let bytes = std::fs::read(&path).with_context(|| format!("no saved session '{name}'"))?;
    let (loaded, meta) = parse_session_bytes(&bytes).context("parsing session file")?;
    *history = loaded;
    // Rebuild BOTH prompt lanes for the CURRENT project + model. The stable lane saved in the file
    // reflects wherever the session was recorded — replaying it verbatim in another checkout
    // grafted the OTHER project's <project_context>/frozen core onto this cwd: every tool ran here
    // while the model was told it was there. The splice keeps the conversation tail (and any
    // handoff seed) intact. Thread-switch resets (todos/cost/grants) are the CALLER's job — this
    // function only loads, so tests and backup paths can use it without mutating global state.
    refresh_prompt_lanes_for_thread_switch(history, model);
    // Cross-project restore is allowed but must be LOUD: name the source so "why does the model
    // think it's in the other repo?" never needs source-diving. Files without provenance stay
    // silent — there is nothing truthful to warn with.
    if let Some(theirs) = meta.as_ref().and_then(|m| m.project_key.as_ref()) {
        if *theirs != config::project_key() {
            let from = meta
                .as_ref()
                .and_then(|m| m.project_root.clone().or_else(|| m.project_slug.clone()))
                .unwrap_or_else(|| "unknown".to_string());
            // If the recorded directory is GONE, "another project" is the wrong accusation: the
            // overwhelmingly likely story is that this very checkout was moved or renamed, so the
            // conversation is the user's own history. Same facts, honest reading — and the phrasing
            // still says what was rebuilt, because the lane rewrite happened either way.
            let vanished = meta
                .as_ref()
                .and_then(|m| m.project_root.as_deref())
                .is_some_and(|r| !std::path::Path::new(r).exists());
            let headline = if vanished {
                format!("⚠ this session was saved at {from}, which no longer exists — moved or renamed project?")
            } else {
                format!("⚠ this session was saved in another project: {from}")
            };
            tui::emit_line(&style(headline).color256(theme::WARN).to_string());
            tui::emit_line(
                &style(format!(
                    "  system context rebuilt for the current project: {}",
                    config::project_root().display()
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
    }
    // Continue autosaving into the SAME file we just restored (don't spawn a fresh slug next turn)
    // — EXCEPT the legacy `last` pointer copy: pinning the live slug to `last` would make every
    // later turn overwrite the pointer instead of a real conversation, so re-home it first.
    let slug = sanitize_name(name);
    if slug == "last" {
        let fresh = allocate_session_slug(history);
        // The transcript is already restored into `history` at this point, so a failed re-home must
        // not fail the restore — report it and leave the slug unpinned, which makes the next autosave
        // allocate a fresh name (and say so) instead of overwriting the pointer.
        match save_session(history, &fresh, Some(model)) {
            Ok(_) => set_session_slug(Some(fresh)),
            Err(e) => {
                set_session_slug(None);
                tui::emit_line(&format!(
                    "{} could not re-home the legacy `last` pointer: {e:#} — this chat will be saved under a new name on the next turn",
                    theme::warn("⚠")
                ));
            }
        }
    } else {
        set_session_slug(Some(slug));
    }
    // Keep the exit-flush snapshot in step with what was just restored, so an abrupt window close
    // right after re-saves this conversation, not a stale one.
    update_live_history(history);
    Ok(conversation_len(history))
}
static SESSION_SLUG: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn session_slug_slot() -> &'static Mutex<Option<String>> {
    SESSION_SLUG.get_or_init(|| Mutex::new(None))
}

fn set_session_slug(slug: Option<String>) {
    let slug = slug.map(|s| sanitize_name(&s));
    *session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = slug.clone();
    crate::core::recovery::set_session_name(slug);
}

fn current_session_slug() -> Option<String> {
    session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
fn pretty_session_name(name: &str) -> String {
    name.replace('-', " ")
}

/// One row of the session pool, as scanned from disk.
struct SessionInfo {
    name: String,
    /// Real conversation turns (leading system lanes excluded); `None` = unreadable/corrupt file —
    /// distinct from a readable empty one, so the picker can say "(unreadable)" instead of "0 msgs".
    msgs: Option<usize>,
    meta: Option<SessionMeta>,
    /// Modification time in Unix MILLIseconds. `None` = the filesystem wouldn't say (network share,
    /// FUSE mount, transient ACL error) — rendered as "age unknown" rather than posing as fresh.
    /// Milliseconds, not seconds, because two saves inside one second are routine (a `/handoff` and
    /// the seeded turn's autosave) and a second-granularity tie fell through to ALPHABETICAL order,
    /// which points the wrong way as often as not while the picker still claims "newest first".
    mtime_ms: Option<u64>,
    /// Saved from THIS project? `None` = no provenance (pre-provenance file, project unknown).
    here: Option<bool>,
}

/// One session file as seen WITHOUT reading it. Statting a directory is cheap; deserializing every
/// multi-MB transcript in it is not — and the startup hint runs before the first prompt is even
/// accepted, so it must not pay for the whole pool just to name one conversation.
struct SessionStat {
    name: String,
    path: std::path::PathBuf,
    mtime_ms: Option<u64>,
    /// Sort key: mtime clamped to now. A stamp in the FUTURE (clock skew from a VM resume, a
    /// pre-NTP boot, a dual-boot clock) would otherwise pin one file to the top of every launch's
    /// offer forever. `None` (filesystem wouldn't say) sorts last.
    recency: Option<u64>,
}

/// The pool, newest first, without reading any transcript. One ordering, shared by the hint and the
/// picker, so the two surfaces can never disagree about which conversation is the newest. The legacy
/// `last.json` pointer is a duplicate COPY of some conversation, not a session of its own — it never
/// appears as a row.
fn stat_sessions() -> Vec<SessionStat> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|x| x.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if name == "last" {
                continue;
            }
            let mtime_ms = e
                .metadata()
                .ok()
                .and_then(|md| md.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            out.push(SessionStat {
                name,
                path,
                mtime_ms,
                recency: None,
            });
        }
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    for s in &mut out {
        s.recency = s.mtime_ms.map(|ms| ms.min(now_ms));
    }
    // Milliseconds, not seconds: two saves inside one second are routine (a `/handoff` and the
    // seeded turn's autosave), and a second-granularity tie fell through to ALPHABETICAL order,
    // which points the wrong way as often as not while the prompt still says "newest first". Name
    // order remains the last resort so the sort is total and stable.
    out.sort_by(|a, b| b.recency.cmp(&a.recency).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Read one row's transcript-derived fields. `(None, None)` = unreadable/corrupt.
fn read_session_row(path: &std::path::Path) -> (Option<usize>, Option<SessionMeta>) {
    match std::fs::read(path)
        .ok()
        .and_then(|b| parse_session_bytes(&b))
    {
        Some((m, meta)) => (Some(conversation_len(&m)), meta),
        None => (None, None),
    }
}

/// Read the whole pool, newest first — for the `/sessions` picker, which shows every row and so
/// genuinely needs every file parsed. The startup hint uses [`most_recent_session`] instead, which
/// parses lazily in the same order.
fn scan_sessions() -> Vec<SessionInfo> {
    let here_key = config::project_key();
    stat_sessions()
        .into_iter()
        .map(|s| {
            let (msgs, meta) = read_session_row(&s.path);
            let here = meta
                .as_ref()
                .and_then(|m| m.project_key.as_ref())
                .map(|k| *k == here_key);
            SessionInfo {
                name: s.name,
                msgs,
                meta,
                mtime_ms: s.mtime_ms,
                here,
            }
        })
        .collect()
}

/// Age of a session file for the picker. Distinct from [`fmt_time_ago`], which was written for
/// `/init --status` and maps both 0 and future stamps to "just now" — for a session row that would
/// print "just now" on an unreadable mtime and on a clock-skewed file, i.e. exactly the two cases
/// the user needs told apart from a genuinely fresh save.
fn fmt_session_age(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "age unknown".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if ms > now_ms.saturating_add(60_000) {
        return "future timestamp (clock skew)".to_string();
    }
    fmt_time_ago((ms / 1000).max(1)) // .max(1): second 0 is fmt_time_ago's "unknown" sentinel
}

/// Age in at most three cells: `now`, `5m`, `19h`, `62d`, `2y`, or `?` when the mtime is unreadable.
///
/// For a LIST, not a status line. `fmt_session_age` spells the unit out, which is right when one age
/// stands alone but wrong down a column: `19 hour(s) ago` against `1 min ago` is a six-character
/// jitter sitting directly in front of the subject, so nothing lines up and the eye has to re-find
/// the text on every row. A clock-skewed file reads `now` rather than announcing the skew — in a
/// 240-row picker that sentence is longer than the row it describes, and the file still sorts first.
fn fmt_session_age_compact(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "?".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let secs = now_ms.saturating_sub(ms) / 1000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=31_535_999 => format!("{}d", secs / 86_400),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Short human label for where a foreign or unlabeled session came from — the picker/hint suffix.
/// Takes the meta rather than the row so the `last.json` re-home path (which has no row) can use it.
///
/// A recorded root that no longer EXISTS is flagged as such rather than presented as a live sibling
/// project: the usual cause is this checkout being moved or renamed, in which case a bare
/// "from <old dir>" is the picker calling the user's own history someone else's.
fn session_origin_label(meta: Option<&SessionMeta>) -> String {
    let root = meta.and_then(|m| m.project_root.as_deref());
    let named = root
        .and_then(|r| std::path::Path::new(r).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .or_else(|| meta.and_then(|m| m.project_slug.clone()));
    match named {
        Some(n) if root.is_some_and(|r| !std::path::Path::new(r).exists()) => {
            format!("from {n} (path gone)")
        }
        Some(n) => format!("from {n}"),
        None => "project unknown".to_string(),
    }
}

/// The session to offer for bare `/resume` and the startup hint: the newest one saved FROM THIS
/// project. Only when this project has none does it fall back to the pool's newest, labeled with
/// its origin — a cross-project resume must be a visible choice, never a trap.
///
/// Returns `(slug, conversation_turns, origin_label)`; the label is `None` for a same-project
/// offer. `None` overall when nothing restorable has been saved yet.
fn most_recent_session() -> Option<(String, usize, Option<String>)> {
    let here_key = config::project_key();
    // Parse in newest-first order and STOP at the first same-project hit, rather than deserializing
    // the whole pool to name one conversation. This runs before the first prompt is accepted, and a
    // long-lived pool is dozens of multi-MB transcripts (autosave per turn + a slug per handoff) —
    // on an AV-scanned or cloud-synced profile dir the eager scan was seconds of silent dead time.
    let stats = stat_sessions();
    let mut rows: Vec<(usize, usize, Option<SessionMeta>)> = Vec::new(); // (index, turns, meta)
    let mut best: Option<usize> = None; // index into `rows` of the best tier seen so far
    let tier = |meta: &Option<SessionMeta>| match meta.as_ref().and_then(|m| m.project_key.as_ref())
    {
        // Three tiers, not two. A file with NO provenance is not evidence of a foreign project — on
        // the first launch after upgrading EVERY file is keyless, so folding `None` in with
        // `Some(false)` made the whole pool "project unknown" and left the prefer-this-project rule
        // dead until each file had been resumed once. Unlabeled ranks between mine and theirs.
        Some(k) if *k == here_key => 0u8,
        None => 1,
        Some(_) => 2,
    };
    for (i, s) in stats.iter().enumerate() {
        let (msgs, meta) = read_session_row(&s.path);
        let Some(turns) = msgs else { continue }; // unreadable/corrupt — never offered
        let t = tier(&meta);
        rows.push((i, turns, meta));
        if best.is_none_or(|b| t < tier(&rows[b].2)) {
            best = Some(rows.len() - 1);
        }
        if t == 0 {
            break; // newest same-project file — nothing later in the order can beat it
        }
    }
    if let Some(b) = best {
        let (i, turns, meta) = &rows[b];
        // Only a file that PROVES it came from elsewhere gets the origin suffix. `None` provenance
        // has nothing truthful to say — the same rule `load_session` applies to its warning.
        let label = (tier(meta) == 2).then(|| session_origin_label(meta.as_ref()));
        return Some((stats[*i].name.clone(), *turns, label));
    }
    // Nothing restorable in the pool — which is NOT the same as an empty pool: one stray unparsable
    // `.json` in the dir used to make this fallback unreachable, hiding a perfectly readable
    // pointer-era transcript from the hint AND (since `last` is never a row) from the picker too.
    // Pre-provenance pool where only the shared `last.json` copy ever existed: re-home that
    // transcript into a real named file once, so it shows up in /sessions from now on.
    let bytes = std::fs::read(sessions_dir().join("last.json")).ok()?;
    let (msgs, carried) = parse_session_bytes(&bytes)?;
    if !msgs.iter().any(|m| m.role == "user") {
        return None;
    }
    let fresh = allocate_session_slug(&msgs);
    // Carry the pointer's own meta through rather than stamping THIS project onto it: the pointer
    // was project-blind, so claiming it as ours would be a lie that also silences load_session's
    // cross-project warning forever. Unattributed → offered as "project unknown", honestly.
    let label = carried
        .as_ref()
        .and_then(|m| m.project_key.as_ref())
        .map_or_else(
            || Some("project unknown".to_string()),
            |k| (*k != config::project_key()).then(|| session_origin_label(carried.as_ref())),
        );
    // Re-homing is a convenience, not a precondition: if the dir is unwritable the transcript
    // is still READABLE, so keep offering it under the legacy `last` name rather than pretending
    // there is nothing to resume. `load_session` re-homes on restore (and reports if that fails).
    match rehome_session(&msgs, &fresh, carried, None) {
        Ok(_) => Some((fresh, conversation_len(&msgs), label)),
        Err(_) => Some(("last".to_string(), conversation_len(&msgs), label)),
    }
}

/// Count of real conversation turns (excluding the leading system lanes) — for the resume hint, so
/// it reports what the user recognizes as "messages" rather than raw vector length.
fn conversation_len(history: &[Message]) -> usize {
    history
        .len()
        .saturating_sub(agent::compact::leading_system_count(history))
}

async fn autosave_session(
    history: &[Message],
    _http: &reqwest::Client,
    _base_url: &str,
    _api_key: &str,
    model: &str,
) {
    autosave_last(history, Some(model));
}

fn delete_session(name: &str) -> Result<()> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}
/// Pick a distinct on-disk slug for a brand-new (unnamed) conversation: the topic suggestion, plus a
/// numeric suffix if a session with that name already exists. Without this every unnamed chat collided
/// on the shared `last` slug and overwrote the previous one, so `/sessions` only ever showed the latest.
fn allocate_session_slug(history: &[Message]) -> String {
    let base = sanitize_name(&suggest_session_name(history));
    let dir = sessions_dir();
    if !dir.join(format!("{base}.json")).exists() {
        return base;
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !dir.join(format!("{cand}.json")).exists() {
            return cand;
        }
    }
    base
}

/// A process-global snapshot of the live conversation, kept fresh so the Windows console control
/// handler (window ✕ / logoff / shutdown) can flush the current chat to disk from its own thread
/// before the process is killed. The main thread never reads it back — it's write-for-the-handler.
static LIVE_HISTORY: OnceLock<Mutex<Vec<Message>>> = OnceLock::new();

fn live_history_slot() -> &'static Mutex<Vec<Message>> {
    LIVE_HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Refresh the snapshot the exit-flush path saves. Called after every user push (mid-turn safety) and
/// at the end of each autosave (so the snapshot is always at least as new as what's on disk).
fn update_live_history(history: &[Message]) {
    *live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = history.to_vec();
}

/// Persist the live conversation on any exit — graceful (/quit, Ctrl-D) or abrupt (window ✕ via the
/// Windows console handler). Safe to call from a foreign thread: it only does synchronous file I/O.
fn flush_live_session_on_exit() {
    // Route the notices for teardown: the "· saving as" line is pointless when the render thread may
    // already be gone, and a failure must go to stderr instead of the TUI. Cleared on the way out so
    // this can't permanently mute a process that keeps running (the unix SIGHUP handler and the
    // test suite both call this without exiting).
    EXIT_FLUSHING.store(true, std::sync::atomic::Ordering::Relaxed);
    let snapshot = live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // No REPL model label on this thread — `save_session` falls back to the configured model.
    autosave_last(&snapshot, None);
    append_memory_stats_sample();
    EXIT_FLUSHING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Append this session's §8 sample to `cli-memory/stats.jsonl`.
///
/// Runs on the exit path rather than per turn for two reasons: the populations are a directory scan
/// (three `load_*` calls) that has no business inside a turn, and one line per session keeps the
/// series readable by hand. The in-process counters are cumulative across the session, so a single
/// line at the end loses nothing but the intra-session shape — which no metric asks about.
///
/// Wholly best-effort. A failure to read the store means no sample, never a failed exit; and
/// `stats::append` itself declines to write when the session ran zero turns.
fn append_memory_stats_sample() {
    let all = memory::store::load_all().unwrap_or_default();
    let live = memory::bloat::supersede::active(&all).len();
    let archived = memory::bloat::caps::list_archive()
        .unwrap_or_default()
        .len();
    let review = memory::store::load_from(&crate::core::config::review_dir())
        .unwrap_or_default()
        .len();
    memory::stats::append(live, archived, all.len().saturating_sub(live), review);
}

/// Set while the exit-flush path is writing, so autosave stays silent during teardown.
static EXIT_FLUSHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mid-turn progress hook hnaded to the agent loop as a plain `fn` pointer (so `AgentConfig` keeps
/// its `Clone + Debug` derives and stays `dyn`-free).
///
/// Without this the exit-flush snapshot only advanced when a turn STARTED or FINISHED: the loop owns
/// `history` mutably for the whole turn, so a terminal closed mid-turn saved the user's question and
/// threw away every assistant reply and tool result the turn had already produced. The loop calls
/// this at each iteration boundary — the same point steering drains, where history is guaranteed
/// coherent (no `tool_calls` awaiting results) — so what lands on disk is always a valid transcript.
/// Memory-only by design; the actual file write happens once, on exit.
fn publish_live_history(history: &[Message]) {
    update_live_history(history);
}

/// Catch the terminal window being closed (✕), user logoff and system shutdown so the live chat is
/// flushed to `/sessions` before the OS terminates us. Ctrl-C / Ctrl-Break are deliberately left to
/// the existing in-app cancel handling (we return FALSE for them, changing nothing).
///
/// Both halves are needed because "the user closed the terminal" is a different OS event per
/// platform: a console control event on Windows, `SIGHUP` (pty hangup) or `SIGTERM` on unix. Without
/// the unix half, closing a terminal there killed the process with no flush at all and the whole
/// conversation was lost — the exact failure this handler exists to prevent.
fn install_exit_flush_handler() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::BOOL;
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
            if matches!(
                ctrl_type,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                // Never let a panic unwind across the FFI boundary into the OS caller.
                let _ = std::panic::catch_unwind(flush_live_session_on_exit);
            }
            0 // FALSE — let the system proceed with default termination in all cases.
        }
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
    // Unix: closing the terminal emulator hangs up the pty (`SIGHUP`); a session manager or
    // `kill` sends `SIGTERM`. Default disposition for both is immediate termination, so the
    // transcript needs saving from the handler task. Watched on the tokio runtime (async signal
    // handling, no `unsafe`), then we exit ourselves — the default action is what the sender asked
    // for, so restoring the terminal and leaving is the honest response to it.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for kind in [SignalKind::hangup(), SignalKind::terminate()] {
            if let Ok(mut sig) = signal(kind) {
                tokio::spawn(async move {
                    if sig.recv().await.is_some() {
                        flush_live_session_on_exit();
                        // Give the alt-screen back before dying, else the user's shell is left in
                        // raw mode with no cursor.
                        tui::deactivate();
                        std::process::exit(0);
                    }
                });
            }
        }
    }
}

/// Best-effort auto-save of the live conversation (called after each turn and on exit) so you can
/// always come back to it via `/sessions` without ever running an explicit save. A brand-new chat is
/// given its own distinct file on first save so it never overwrites another unnamed conversation.
fn autosave_last(history: &[Message], model: Option<&str>) {
    if history.iter().any(|m| m.role == "user") {
        let name = match current_session_slug() {
            Some(n) => n,
            None => {
                let slug = allocate_session_slug(history);
                set_session_slug(Some(slug.clone()));
                // Name the file OUT LOUD the moment it exists: "which file is THIS conversation
                // being written to?" must be answerable from the screen, not from source.
                if !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                    tui::emit_line(&style(format!("· saving as “{slug}”")).dim().to_string());
                }
                slug
            }
        };
        // "auto-saves as you go" is a PROMISE (/help says so). When the write fails — full disk,
        // ACL damage, OneDrive/AV lock on ~/.aizen — swallowing it meant the user worked for hours
        // believing the transcript was on disk and found nothing to resume. Say it once per failure
        // streak (not every turn), and say it again after a recovery so the state is never stale.
        match save_session(history, &name, model) {
            Ok(_) => {
                if AUTOSAVE_BROKEN.swap(false, std::sync::atomic::Ordering::Relaxed)
                    && !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed)
                {
                    tui::emit_line(
                        &style("· autosave recovered — this conversation is being saved again")
                            .dim()
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                if !AUTOSAVE_BROKEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let msg = format!(
                        "⚠ autosave failed: {e:#} — this conversation is NOT being saved. Fix the path or use /sessions to save elsewhere."
                    );
                    // The TUI may already be torn down on the exit-flush path; stderr still lands.
                    if EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("{msg}");
                    } else {
                        tui::emit_line(&style(msg).color256(theme::WARN).to_string());
                    }
                }
            }
        }
        update_live_history(history);
        let _ = crate::core::recovery::checkpoint_history(
            history,
            None,
            crate::core::recovery::RecoveryPhase::Idle,
        );
    }
}

/// Latch so a persistent autosave failure warns ONCE per streak instead of every turn (and reports
/// once when writes start working again).
static AUTOSAVE_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
                Err(e) => eprintln!("{} {e}", style("restore:").red()),
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
                    Err(e) => eprintln!("{} {e}", style("save:").red()),
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
                    Err(e) => eprintln!("{} {e}", style("delete:").red()),
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
    let idx = model_default_index(&ids, cli_config::load().model.as_deref());
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
    let model = model
        .or_else(|| cli_config::branded_env("MODEL"))
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

fn http_client() -> Result<reqwest::Client> {
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
            let fut =
                client::chat_with_tools(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
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
        if acted > 0 {
            tui::emit_line(
                &style(format!(
                    "⚖ reconciled {acted} memory fact(s) — `aizen memory list --superseded` to review, `revive <id>` to undo"
                ))
                .dim()
                .to_string(),
            );
        }
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

async fn run_config(cmd: Option<ConfigCmd>) -> Result<()> {
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
            model_endpoint,
        } => {
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
                && subagent_model.is_none()
                && subagent_base_url.is_none()
                && subagent_api_key_ref.is_none()
                && model_endpoint.is_empty()
            {
                anyhow::bail!("nothing to set — pass at least one supported --flag (including --timemachine-keep / --timemachine-max-files / --timemachine-max-bytes / --timemachine-max-file-bytes)");
            }
            let mut cfg = cli_config::load();
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
            // Sub-agent default endpoint (`roles.subagent_default`): set any of model/base_url/
            // api_key_ref; an empty string CLEARS that field. Editing any sub-field materializes the
            // `roles` + `subagent_default` objects; clearing every field drops `subagent_default`.
            if subagent_model.is_some()
                || subagent_base_url.is_some()
                || subagent_api_key_ref.is_some()
            {
                let mut roles = cfg.roles.take().unwrap_or_default();
                let mut sd = roles.subagent_default.take().unwrap_or_default();
                let apply = |slot: &mut Option<String>, v: Option<String>| {
                    if let Some(s) = v {
                        let s = s.trim();
                        *slot = if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        };
                    }
                };
                apply(&mut sd.model, subagent_model);
                apply(&mut sd.base_url, subagent_base_url);
                apply(&mut sd.api_key_ref, subagent_api_key_ref);
                roles.subagent_default =
                    (sd.model.is_some() || sd.base_url.is_some() || sd.api_key_ref.is_some())
                        .then_some(sd);
                cfg.roles = roles.has_any().then_some(roles);
            }
            // Model→endpoint registry: each `--model-endpoint` is `model[,base_url=URL][,api_key_ref=…]`;
            // a bare model id or `model,clear` removes the entry.
            for spec in model_endpoint {
                apply_model_endpoint(&mut cfg, &spec)?;
            }
            cli_config::save(&cfg)?;
            println!(
                "{} {}",
                crate::ui::theme::ok("✓"),
                style("saved").color256(splash::ACCENT)
            );
            print_config(&cfg);
            Ok(())
        }
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

/// Render the saved config as a grouped, aligned "Studio" panel: a gold title rule with the file
/// path, then sections (Endpoint / Session / Cost / Display) of `key   value` rows where the value's
/// colour carries meaning (gold = a chosen value, green = on/ok, faint = off/unset). Shown at the end
/// of the wizard, after `config set`, and on `aizen config show`. Plain `println!` (not the sticky
/// emit): it always runs outside the pinned footer (suspended menu / one-shot CLI), and `console`
/// auto-strips the colour under `NO_COLOR`/pipes.
fn print_config(cfg: &cli_config::CliConfig) {
    let width = tui::width().clamp(46, 72);
    let path = cli_config::config_path().display().to_string();

    // ── header: "config" on the left, the file path faint on the right, then a gold rule ──
    let title = "config";
    let used = console::measure_text_width(title) + console::measure_text_width(&path);
    let gap = width.saturating_sub(used + 2).max(1);
    println!(
        "\n  {}{}{}",
        theme::accent(title).bold(),
        " ".repeat(gap),
        theme::faint(&path)
    );
    println!("  {}", theme::accent_dim("─".repeat(width)));

    // row/section helpers — keys aligned in a fixed column, values free-form (already styled).
    let section =
        |name: &str| println!("\n  {} {}", theme::accent("◆"), theme::accent(name).bold());
    let row = |key: &str, val: String| println!("    {}  {val}", theme::muted(format!("{key:<8}")));
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
    let redact_url = |u: &str| -> String {
        match url::Url::parse(u) {
            Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
                let _ = parsed.set_username("•••");
                let _ = parsed.set_password(None);
                parsed.to_string()
            }
            _ => u.to_string(),
        }
    };

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
fn model_default_index(models: &[String], current: Option<&str>) -> usize {
    current
        .and_then(|m| models.iter().position(|x| x == m))
        .unwrap_or(0)
}

const CUSTOM_MODEL_ITEM: &str = "‹ type a custom id ›";

/// `aizen config` / `/config` / menu → Setup. A fresh install (no endpoint yet) gets the guided
/// linear setup once so nothing required is missed; an already-configured user gets the HUB menu —
/// jump to the one section you want, edit just it, it saves on the spot, you're back at the menu. No
/// more Enter-through-every-prompt to change a single field.
async fn config_wizard() -> Result<()> {
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
const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        label: "OpenAI",
        base: "https://api.openai.com/v1",
        keys_url: "https://platform.openai.com/api-keys",
        sample_model: "gpt-4o",
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

/// `  ✓ <msg>` in the ok colour.
fn line_ok(msg: &str) {
    println!("  {} {}", crate::ui::theme::ok("✓"), style(msg).dim());
}

/// `  ✗ <msg>` in red — a failure the user has to act on.
fn line_bad(msg: &str) {
    println!("  {} {}", style("✗").red(), style(msg).red());
}

/// `  ! <msg>` in the warn colour — something to know, but not a stop.
fn line_warn(msg: &str) {
    println!(
        "  {} {}",
        style("!").color256(crate::ui::theme::WARN),
        style(msg).color256(crate::ui::theme::WARN)
    );
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
                    println!("    {}", style(&detail).dim());
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
                    println!("    {}", style(&detail).dim());
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
        println!("  {}", style(format!("get a key: {url}")).dim());
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
                    println!("    {}", style(&detail).dim());
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
        let key_h = if cfg.api_key.is_some() {
            "set"
        } else {
            "missing"
        };
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
            format!("Connection      · api key {key_h}"),
            format!("Model & context · {model_h}"),
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
        // Sections 0..=7 edit + save; 8 shows the panel; 9 (or Esc) exits.
        let edited = match pick {
            0 => config_edit_connection(&mut cfg).await,
            1 => config_edit_model(&mut cfg).await,
            2 => config_edit_websearch(&mut cfg).await,
            3 => config_edit_memory(&mut cfg),
            4 => config_edit_session(&mut cfg),
            5 => config_edit_reasoning(&mut cfg),
            6 => config_edit_approval(&mut cfg),
            7 => config_edit_display(&mut cfg),
            8 => {
                print_config(&cfg);
                continue;
            }
            _ => break,
        };
        match edited {
            Ok(()) => match cli_config::save(&cfg) {
                Ok(_) => println!(
                    "  {} {}",
                    crate::ui::theme::ok("✓"),
                    style("saved").color256(splash::ACCENT)
                ),
                Err(e) => eprintln!("  {} {e}", style("save:").red()),
            },
            Err(e) => eprintln!("  {} {e}", style("config:").red()),
        }
    }
    Ok(())
}

/// Section editor: provider → base URL → API key, each step verified against the live endpoint
/// before it is accepted. Nothing is written to `cfg` until a step actually passes, so a failed
/// attempt leaves the previous working connection intact.
async fn config_edit_connection(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let http = http_client()?;

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
    cfg.base_url = Some(base.clone());

    let keys_url = preset.map(|p| p.keys_url);
    match prompt_validated_api_key(&theme, &http, &base, cfg.api_key.as_deref(), keys_url).await? {
        Some((key, fetched)) => {
            cfg.api_key = Some(key);
            if !fetched.is_empty() {
                infos = fetched;
            }
        }
        None => return Ok(()),
    }

    // The key check already fetched the list — offering it here saves a redundant round-trip and
    // means a fresh connection lands on a working model instead of whatever was configured before.
    if !infos.is_empty() && yn(&theme, "Pick a model now?", true)? {
        pick_model_from(&theme, cfg, &infos, preset.map(|p| p.sample_model))?;
    }
    Ok(())
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
        .map(|m| match m.context_length {
            Some(n) => format!("{}  ({} ctx)", m.id, n),
            None => m.id.clone(),
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

/// Section editor: fetch the model list, pick one (Esc keeps current), then the context window.
async fn config_edit_model(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let theme = ui_theme();
    let (base, key) = match (cfg.base_url.clone(), cfg.api_key.clone()) {
        (Some(b), Some(k)) => (b, k),
        _ => {
            println!(
                "  {}",
                style("set the Connection (base URL + key) first").color256(crate::ui::theme::WARN)
            );
            return Ok(());
        }
    };
    let http = http_client()?;
    print!("{} {base} … ", style("Fetching models from").dim());
    std::io::Write::flush(&mut std::io::stdout()).ok();
    match client::fetch_models_info(&http, &base, &key).await {
        Ok(infos) if !infos.is_empty() => {
            println!("{}", style(format!("ok ({} found)", infos.len())).dim());
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
                Ok(_) => println!("{}", style("no models returned.").dim()),
                Err(e) => println!("{}", style(format!("failed: {e}")).red()),
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
        println!(
            "{}",
            style(format!(
                "Context window — currently {shown} tokens ({note})."
            ))
            .dim()
        );
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
    println!("  {}", style(format!("get a key: {keys_url}")).dim());
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
    println!(
        "{}",
        style("web_search is keyed-only: without a key it returns an error rather than guessing.")
            .dim()
    );
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
        println!(
            "  {}",
            style("(a `--features dense` build adds embedding-based recall for paraphrases)").dim()
        );
        return Ok(());
    }
    if let Ok(v) = std::env::var("AIZEN_MEM_DENSE") {
        line_warn(&format!(
            "$AIZEN_MEM_DENSE={v} overrides the dense decision below"
        ));
    }
    if models.is_empty() {
        line_warn("no embedding model installed — dense recall is off");
        println!(
            "  {}",
            style("get one with: aizen memory model-download").dim()
        );
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
    println!();
    println!("{}", style("Aizen · setup").bold().color256(splash::ACCENT));
    println!(
        "{}",
        style(cli_config::config_path().display()).color256(crate::ui::theme::FAINT)
    );
    println!(
        "{}",
        style("Enter keeps the shown default at each step · Ctrl-C cancels")
            .color256(crate::ui::theme::FAINT)
    );
    println!(
        "{}",
        style("─".repeat(width)).color256(crate::ui::theme::ACCENT_DIM)
    );
    // Group the steps under gold section headers so the flow reads as Connection → Model → Behavior.
    let step = |label: &str| {
        println!(
            "\n{} {}",
            style("◆").color256(splash::ACCENT),
            style(label).color256(splash::ACCENT).bold()
        );
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
    println!(
        "{}",
        style(format!(
            "Context window — currently {shown} tokens ({note})."
        ))
        .dim()
    );
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
    println!(
        "{}",
        style("Optional. web_search is keyed-only — skip now and add one later with `/config`.")
            .dim()
    );
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
    println!(
        "\n{} {}",
        crate::ui::theme::ok("✓"),
        style("Saved.").color256(splash::ACCENT).bold()
    );
    print_config(cfg);
    println!(
        "{}",
        style("Ready — type a message, or run:  aizen chat -p \"hello\"")
            .color256(crate::ui::theme::FAINT)
    );
    Ok(())
}

async fn run_models(args: ModelsArgs) -> Result<()> {
    let (base_url, api_key) = resolve_base_key(args.base_url, args.api_key)?;
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
        let ctx = match m.context_length {
            Some(n) if n >= 1000 => format!("  · ctx {}K", n / 1000),
            Some(n) => format!("  · ctx {n}"),
            None => String::new(),
        };
        println!("{}{ctx}{mark}", m.id);
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
        let fut = client::chat_with_tools(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
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
        MemoryCmd::List { scope, superseded } => {
            if superseded {
                memory::cmd_list_superseded()
            } else {
                memory::cmd_list(scope.as_deref())
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
        PersonaCmd::SelfMem { name } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass a name or `aizen persona use <name>`")
                })?,
            };
            let label = persona::load(&slug)
                .map(|p| p.name)
                .unwrap_or_else(|| slug.clone());
            persona_self_view(&slug, &label);
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
                // Voyager provenance (v{N} · {M}× · updated …) — empty for a pristine, never-used v1.
                let prov = skill::version_tag(s);
                let prov = if prov.is_empty() {
                    String::new()
                } else {
                    format!("  ({prov})")
                };
                println!("{}{tag}{prov}  —  {}", s.name, d);
            }
            if all_zones {
                let others = skill::list_other_zones();
                if !others.is_empty() {
                    println!("\nother workspaces' zones (invisible here):");
                    for (zone, s) in &others {
                        println!("{}  [p:{zone}]  —  {}", s.name, s.description);
                    }
                }
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
                println!("deleted '{name}'");
            } else {
                println!("(no skill named '{name}')");
            }
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
            println!("  {} {}  —  {}", mark, a.slug(), desc.replace('\n', " "));
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
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        vec![
            "opus-4-8".to_string(),
            "sonnet-4-6".to_string(),
            "minimax-m3".to_string(),
        ]
    }

    /// The `/v1` hint fires only when the URL genuinely lacks a version segment. Both directions
    /// matter: no hint on an already-versioned URL (suggesting `/v1/v1` sends the user in circles),
    /// and a hint whenever the last segment is not `v<digits>` — `/api` and `/openai` are paths, not
    /// versions, which is exactly the case that leaves people stuck on a 404 with nothing to try.
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

    /// `/compact` used to `await` its summarizer call bare inside the REPL loop: no token armed, so
    /// `turn_in_flight()` was false and Esc merely cleared the draft, while the REPL sat blocked in
    /// the await consuming nothing. A hung endpoint froze the app until the 300s read timeout. The
    /// wrapper must (a) report in-flight so Esc routes to cancel, (b) actually return on cancel
    /// rather than waiting out the call, and (c) leave the slot clean for the next turn.
    #[tokio::test]
    async fn cancellable_slash_lets_esc_abort_a_hung_model_call() {
        let _g = tui::TEST_CANCEL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A call that never returns — stands in for a dead endpoint inside the read timeout.
        let hung = async {
            std::future::pending::<()>().await;
            unreachable!("the wrapper must not wait for this to finish");
        };
        // Press Esc once the wrapper has armed its token: this is exactly what the input thread does.
        let presser = tokio::spawn(async {
            for _ in 0..200 {
                if tui::turn_in_flight() {
                    tui::request_cancel();
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            false
        });
        let out = cancellable_slash(hung).await;
        assert!(
            presser.await.unwrap(),
            "the wrapper must report in-flight so Esc means cancel"
        );
        assert!(
            out.is_none(),
            "cancel must win the race instead of blocking on the call"
        );
        assert!(
            !tui::turn_in_flight(),
            "the token is disarmed on the way out, so Esc goes idle again"
        );
    }

    /// A settings change mid-chat must not end the conversation. `/config` and `/model` used to route
    /// through `rebuild_system`, whose `seed_prompt_lanes` starts with `history.clear()` — so going to
    /// config to retune the context and coming back left an empty thread.
    #[test]
    fn refreshing_prompt_lanes_keeps_the_conversation() {
        let mut history = vec![
            Message::system("STABLE LANE v1".to_string()),
            Message::system("dynamic lane v1".to_string()),
            Message::user("câu hỏi đầu tiên".to_string()),
            Message::assistant("trả lời đầu tiên".to_string()),
            Message::user("câu hỏi thứ hai".to_string()),
        ];
        let before: Vec<_> = history
            .iter()
            .filter(|m| m.role != "system")
            .cloned()
            .collect();

        refresh_prompt_lanes_in_place(&mut history, "opus-4-8");

        // Every non-system message survives, in order.
        let after: Vec<_> = history
            .iter()
            .filter(|m| m.role != "system")
            .cloned()
            .collect();
        assert_eq!(
            after.len(),
            before.len(),
            "conversation dropped: {history:#?}"
        );
        for (a, b) in after.iter().zip(before.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content);
        }
        // The stale lanes are gone (rewritten, not appended) and the leading block is still systems.
        let lead = agent::compact::leading_system_count(&history);
        assert!(
            (1..=2).contains(&lead),
            "expected 1-2 system lanes, got {lead}"
        );
        assert!(
            !history[..lead]
                .iter()
                .any(|m| m.content.as_deref() == Some("STABLE LANE v1")),
            "old stable lane not replaced"
        );
        assert_eq!(
            history[lead].role, "user",
            "conversation must start right after the lanes"
        );

        // And the contrast: the /clear path is still allowed to wipe.
        let mut fresh = history.clone();
        rebuild_system(&mut fresh, "opus-4-8");
        assert!(
            !fresh.iter().any(|m| m.role != "system"),
            "rebuild_system is the /clear path and must still reset"
        );
    }

    /// `/resume` and the startup hint must offer THIS project's newest session, not whichever
    /// project's conversation happened to write last — the shared flat pool is exactly how a
    /// foreign transcript used to be offered unlabeled and restored into the wrong repo.
    #[test]
    fn most_recent_session_prefers_this_project_over_a_newer_foreign_one() {
        // Serialize with every home-MUTATING test (zones/skills/memory sandboxes repoint
        // AIZEN_HOME then delete their tree) — this test's saves resolve through the home.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-recent-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();

        // A HERE session, saved through the real writer (stamps the current project key)…
        let history = vec![
            Message::system("lane".to_string()),
            Message::user("here-work".to_string()),
        ];
        save_session(&history, "zzz-here", Some("m1")).unwrap();
        // …and a FOREIGN session. Named to sort as "newer" under the scan's equal-mtime
        // name tie-break, so this test can't pass by timing luck. Its root EXISTS on disk, which
        // is the ordinary case (two live checkouts): the label names the dir with no caveat.
        let foreign_root = home.join("else");
        std::fs::create_dir_all(&foreign_root).unwrap();
        let foreign = serde_json::json!({
            "version": 2,
            "meta": {
                "project_key": "c:/somewhere/else",
                "project_root": foreign_root.display().to_string(),
            },
            "messages": [
                { "role": "system", "content": "lane" },
                { "role": "user", "content": "foreign-work" },
            ]
        });
        std::fs::write(
            dir.join("aaa-foreign.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();

        let (slug, n, origin) = most_recent_session().expect("a saved session must be found");
        assert_eq!(
            slug, "zzz-here",
            "must prefer this project's session over a foreign one"
        );
        assert_eq!(
            n, 1,
            "hint counts conversation turns, not raw vector length"
        );
        assert!(
            origin.is_none(),
            "a same-project offer carries no origin label"
        );

        // With the here-session gone, the foreign one IS offered — but labeled with its origin.
        std::fs::remove_file(dir.join("zzz-here.json")).unwrap();
        let (slug, _, origin) = most_recent_session().expect("foreign fallback must be offered");
        assert_eq!(slug, "aaa-foreign");
        assert_eq!(
            origin.as_deref(),
            Some("from else"),
            "a foreign offer must name its project"
        );

        // And when that project's dir is GONE (deleted or renamed checkout), the label says so
        // instead of naming a path the user can no longer go look at.
        std::fs::remove_dir_all(&foreign_root).unwrap();
        let (_, _, origin) = most_recent_session().expect("foreign fallback must be offered");
        assert_eq!(
            origin.as_deref(),
            Some("from else (path gone)"),
            "a vanished origin must be flagged, not presented as a live project"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every saved file must carry provenance (project key/root/slug + timestamps), `created` must
    /// survive re-saves, and a pre-provenance bare-array file must still load.
    #[test]
    fn session_files_carry_provenance_and_legacy_arrays_still_load() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-sess-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();

        let history = vec![
            Message::system("lane".to_string()),
            Message::user("stamp me".to_string()),
        ];
        save_session(&history, "stamped", Some("model-x")).unwrap();
        let bytes = std::fs::read(dir.join("stamped.json")).unwrap();
        let (_, meta) = parse_session_bytes(&bytes).expect("v2 file parses");
        let meta = meta.expect("v2 file carries meta");
        assert_eq!(
            meta.project_key.as_deref(),
            Some(config::project_key().as_str())
        );
        assert_eq!(meta.model.as_deref(), Some("model-x"));
        let created = meta.created.clone().expect("created stamped");
        // Re-save: `created` is the file's birth stamp and must not advance.
        save_session(&history, "stamped", Some("model-x")).unwrap();
        let bytes = std::fs::read(dir.join("stamped.json")).unwrap();
        let (_, meta2) = parse_session_bytes(&bytes).unwrap();
        assert_eq!(meta2.unwrap().created.as_deref(), Some(created.as_str()));

        // Legacy: a bare `Vec<Message>` array (what every pre-provenance save wrote).
        let legacy = serde_json::json!([
            { "role": "system", "content": "old lane" },
            { "role": "user", "content": "legacy question" },
        ]);
        std::fs::write(
            dir.join("old-chat.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let mut restored = Vec::new();
        let n = load_session(&mut restored, "old-chat", "model-x").unwrap();
        assert_eq!(n, 1, "legacy conversation still loads");
        assert!(restored
            .iter()
            .any(|m| m.content.as_deref() == Some("legacy question")));

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Restoring a session saved elsewhere must REBUILD the system lanes for the current project:
    /// keeping the file's own stable lane grafted the other project's context onto this cwd, and
    /// the model confidently edited the wrong tree.
    #[test]
    fn load_session_rebuilds_stale_lanes_for_the_current_project() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-sess-lanes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let stale = "STALE STABLE LANE recorded in another checkout";
        let foreign = serde_json::json!({
            "version": 2,
            "meta": { "project_key": "c:/somewhere/else", "project_root": "C:/somewhere/else" },
            "messages": [
                { "role": "system", "content": stale },
                { "role": "user", "content": "carried question" },
            ]
        });
        std::fs::write(
            dir.join("from-b.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();

        let mut history = Vec::new();
        let n = load_session(&mut history, "from-b", "model-x").unwrap();
        assert_eq!(n, 1);
        assert!(
            !history.iter().any(|m| m.content.as_deref() == Some(stale)),
            "the foreign stable lane must be replaced, not replayed"
        );
        assert_eq!(
            history[0].role, "system",
            "current-project lanes lead the restored thread"
        );
        assert!(
            history
                .iter()
                .any(|m| m.content.as_deref() == Some("carried question")),
            "the conversation itself is preserved"
        );

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The pool scan must classify every file shape it can meet: this project's v2 file, another
    /// project's v2 file, a pre-provenance bare array (project unknown), and a corrupt file — which
    /// must read as UNREADABLE, never as a plausible empty conversation the user might restore.
    #[test]
    fn scan_sessions_classifies_mine_foreign_unlabeled_and_corrupt() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-scan-classes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();

        save_session(
            &[Message::system("lane"), Message::user("mine")],
            "mine",
            Some("m"),
        )
        .unwrap();
        // A live foreign checkout: its root exists, so the label names it without a caveat. (The
        // vanished-root variant is covered by the most_recent_session test.)
        let foreign_root = home.join("repo");
        std::fs::create_dir_all(&foreign_root).unwrap();
        let foreign = serde_json::json!({
            "version": 2,
            "meta": {
                "project_key": "c:/other/repo",
                "project_root": foreign_root.display().to_string(),
            },
            "messages": [{ "role": "user", "content": "theirs" }]
        });
        std::fs::write(
            dir.join("theirs.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("unlabeled.json"),
            br#"[{"role":"user","content":"old"}]"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.json"), b"{not json").unwrap();
        // The retired pointer must never appear as a restorable row.
        std::fs::write(
            dir.join("last.json"),
            br#"[{"role":"user","content":"ptr"}]"#,
        )
        .unwrap();

        let pool = scan_sessions();
        let by = |n: &str| pool.iter().find(|s| s.name == n).expect("row present");
        assert!(
            !pool.iter().any(|s| s.name == "last"),
            "the pointer is not a session row"
        );
        assert_eq!(by("mine").here, Some(true));
        assert_eq!(by("theirs").here, Some(false));
        assert_eq!(
            by("unlabeled").here,
            None,
            "no provenance → project unknown, not 'foreign'"
        );
        assert_eq!(
            by("broken").msgs,
            None,
            "a corrupt file is unreadable, not empty"
        );
        assert_eq!(
            session_origin_label(by("theirs").meta.as_ref()),
            "from repo"
        );
        assert_eq!(
            session_origin_label(by("unlabeled").meta.as_ref()),
            "project unknown"
        );

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The picker's age column must tell "I couldn't read the clock" and "this file claims the
    /// future" APART from "saved just now" — the three used to render identically, so an unreadable
    /// or clock-skewed row looked like the freshest conversation in the pool.
    #[test]
    fn session_age_distinguishes_unknown_skewed_and_real_stamps() {
        assert_eq!(
            fmt_session_age(None),
            "age unknown",
            "no mtime is not 'just now'"
        );

        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        assert_eq!(
            fmt_session_age(Some(now_ms + 3_600_000)),
            "future timestamp (clock skew)",
            "a stamp beyond the skew grace must be called out, not sorted to the top silently"
        );
        // Just inside the grace window: ordinary filesystem/clock jitter still reads as fresh.
        assert!(!fmt_session_age(Some(now_ms + 5_000)).contains("clock skew"));

        // Epoch 0 must not collapse into fmt_time_ago's "unknown" sentinel.
        assert_ne!(fmt_session_age(Some(0)), "age unknown");
        let hour_ago = fmt_session_age(Some(now_ms.saturating_sub(3_600_000)));
        assert!(
            hour_ago.contains('h') || hour_ago.contains("hour"),
            "real age renders: {hour_ago}"
        );
    }

    /// The compact age is a COLUMN, so its width is the contract: three cells, always. The verbose
    /// `fmt_session_age` is right for a status line and wrong here — "future timestamp (clock skew)"
    /// is 30 characters of prose sitting in front of the only field the user reads.
    #[test]
    fn compact_age_always_fits_three_cells() {
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        // In order: unreadable, now, skewed into the future, 5m, 19h, 62d, epoch (years).
        for ms in [
            None,
            Some(now_ms),
            Some(now_ms + 3_600_000),
            Some(now_ms.saturating_sub(5 * 60_000)),
            Some(now_ms.saturating_sub(19 * 3_600_000)),
            Some(now_ms.saturating_sub(62 * 86_400_000)),
            Some(0),
        ] {
            let s = fmt_session_age_compact(ms);
            assert!(
                s.chars().count() <= 3 && !s.is_empty(),
                "{ms:?} → {s:?} must fit the 3-cell column"
            );
        }
        assert_eq!(fmt_session_age_compact(None), "?");
        assert_eq!(
            fmt_session_age_compact(Some(now_ms.saturating_sub(19 * 3_600_000))),
            "19h"
        );
    }

    /// Save-as must refuse `last`: the picker skips that stem, so accepting it printed "saved" for a
    /// file the user could then neither restore nor delete, and pinned every later autosave to it.
    #[test]
    fn save_as_refuses_the_retired_pointer_name() {
        assert!(session_save_name_error("last").is_some());
        assert!(
            session_save_name_error("  last  ").is_some(),
            "trimmed before the check"
        );
        assert!(
            session_save_name_error("LAST").is_none(),
            "case-distinct stems are distinct files"
        );
        assert!(
            session_save_name_error("lastly").is_none(),
            "only the exact name is reserved"
        );
        // Punctuation sanitizes to a DIFFERENT stem (`last_`), which is its own listable file.
        assert!(session_save_name_error("last!").is_none());
        assert!(session_save_name_error("fix-the-parser").is_none());
    }

    /// `aizen zone migrate` must re-home SESSIONS too. They are the one artifact keyed by provenance
    /// inside the file rather than by directory, so the slug-directory sweep was blind to them: after
    /// a rename/move every one of the user's own transcripts stayed labeled "from <old dir>" forever.
    #[test]
    fn zone_migrate_rehomes_sessions_carrying_a_legacy_slug() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-zone-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let legacy_slug = "zzz-legacy-slug";
        let file = serde_json::json!({
            "version": 2,
            "meta": {
                "project_key": "c:/old/checkout",
                "project_root": "C:/old/checkout",
                "project_slug": legacy_slug,
                "created": "2026-01-01T00:00:00+00:00",
                "updated": "2026-01-02T00:00:00+00:00",
            },
            "messages": [
                { "role": "system", "content": "lane" },
                { "role": "user", "content": "pre-move work" },
            ]
        });
        std::fs::write(dir.join("moved.json"), serde_json::to_vec(&file).unwrap()).unwrap();
        // An unrelated row must be left strictly alone.
        save_session(
            &[Message::system("lane"), Message::user("mine")],
            "mine",
            Some("m"),
        )
        .unwrap();

        assert_eq!(
            count_sessions_of_slug(legacy_slug),
            1,
            "the plan must see the session"
        );
        assert_eq!(count_sessions_of_slug("no-such-slug"), 0);

        let mut errs: Vec<String> = Vec::new();
        let n = retag_sessions_of_slug(legacy_slug, &mut |e| errs.push(e));
        assert_eq!(
            (n, errs.len()),
            (1, 0),
            "exactly the legacy row is re-homed, cleanly"
        );

        let pool = scan_sessions();
        let moved = pool
            .iter()
            .find(|s| s.name == "moved")
            .expect("row survives");
        assert_eq!(
            moved.here,
            Some(true),
            "the transcript now reads as this project's own"
        );
        assert_eq!(moved.msgs, Some(1), "the conversation itself is untouched");
        let meta = moved.meta.as_ref().expect("provenance rewritten");
        assert_eq!(
            meta.project_slug.as_deref(),
            Some(config::project_slug().as_str())
        );
        // The aging clock must NOT be reset by a bookkeeping rewrite.
        assert_eq!(meta.updated.as_deref(), Some("2026-01-02T00:00:00+00:00"));
        assert_eq!(meta.created.as_deref(), Some("2026-01-01T00:00:00+00:00"));

        assert_eq!(
            count_sessions_of_slug(legacy_slug),
            0,
            "migration is idempotent-complete"
        );

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A restored legacy `last` pointer must be RE-HOMED into a real named file: pinning the live
    /// slug to `last` made every later turn overwrite the pointer instead of a conversation.
    #[test]
    fn restoring_the_legacy_last_pointer_rehomes_it_to_a_named_file() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-last-rehome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();

        // A pre-provenance pool whose ONLY file is the shared pointer.
        let legacy = serde_json::json!([
            { "role": "system", "content": "old lane" },
            { "role": "user", "content": "pointer-era chat" },
        ]);
        std::fs::write(dir.join("last.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();

        // The hint must offer it (not "nothing to resume") under a NEW name…
        let (slug, n, _) = most_recent_session().expect("the legacy pointer must still be offered");
        assert_ne!(
            slug, "last",
            "the offer must be re-homed, not the pointer itself"
        );
        assert_eq!(n, 1);
        assert!(
            dir.join(format!("{slug}.json")).exists(),
            "re-homed file was written"
        );

        // …and restoring the pointer directly must never pin the live slug to `last`.
        let mut history = Vec::new();
        load_session(&mut history, "last", "model-x").unwrap();
        assert_ne!(
            current_session_slug().as_deref(),
            Some("last"),
            "the live slug must never be the pointer"
        );

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The `/handoff` seed is conversation content, not prompt prefix: a lane rewrite (what
    /// `/config` and `/model` do via `refresh_prompt_lanes_in_place`) must splice AROUND it.
    /// Before the marker, the splice consumed it and the fresh thread silently lost its context.
    #[test]
    fn handoff_seed_survives_a_lane_rewrite() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-handoff-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);

        let seed = format!(
            "{}\ndecisions: use the v2 format",
            agent::compact::HANDOFF_MARKER_PREFIX
        );
        let mut history = vec![
            Message::system("stable lane".to_string()),
            Message::system("dynamic lane".to_string()),
            Message::system(seed.clone()),
            Message::user("continue the migration".to_string()),
        ];
        assert_eq!(
            agent::compact::leading_system_count(&history),
            2,
            "the seed must not count as prompt prefix"
        );
        refresh_prompt_lanes_in_place(&mut history, "model-x");
        assert!(
            history
                .iter()
                .any(|m| m.content.as_deref() == Some(seed.as_str())),
            "a /config-style lane rewrite must keep the handoff seed"
        );
        assert!(
            history
                .iter()
                .any(|m| m.content.as_deref() == Some("continue the migration")),
            "the conversation tail survives too"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The mid-turn publish hook is what makes a terminal closed DURING a turn keep the work: the
    /// agent loop owns `history` for the whole turn, so without it the exit snapshot stayed frozen at
    /// the user's question and every reply/tool result produced so far was lost.
    #[test]
    fn publishing_mid_turn_advances_the_exit_snapshot() {
        let early = vec![
            Message::system("lane".to_string()),
            Message::user("làm việc đi".to_string()),
        ];
        publish_live_history(&early);
        let snap_before = live_history_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();

        // The turn progresses: assistant reply + a tool result land while the loop still owns history.
        let mut mid = early.clone();
        mid.push(Message::assistant("đang chạy".to_string()));
        mid.push(Message::tool_result("call-1", "kết quả"));
        publish_live_history(&mid);

        let snap_after = live_history_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(snap_before, early.len());
        assert_eq!(
            snap_after.len(),
            mid.len(),
            "mid-turn progress never reached the exit snapshot"
        );
        assert_eq!(
            snap_after.last().unwrap().content.as_deref(),
            Some("kết quả")
        );
    }

    /// A legacy single-lane history (persisted before the split) must gain both lanes without
    /// losing the chat — `splice(0..1, …)` grows the leading block in place.
    #[test]
    fn refreshing_prompt_lanes_migrates_a_legacy_single_lane() {
        let mut history = vec![
            Message::system("legacy combined prompt".to_string()),
            Message::user("giữ tôi lại".to_string()),
        ];
        refresh_prompt_lanes_in_place(&mut history, "opus-4-8");
        let lead = agent::compact::leading_system_count(&history);
        assert_eq!(history[lead].content.as_deref(), Some("giữ tôi lại"));
        assert!(
            !history
                .iter()
                .any(|m| m.content.as_deref() == Some("legacy combined prompt")),
            "legacy lane should be replaced, not kept"
        );
    }

    #[test]
    fn classify_health_probe_rules() {
        // Ok + fast → green.
        assert_eq!(
            classify_health_probe(Ok(std::time::Duration::from_millis(500))),
            tui::HealthKind::Ok
        );
        // Ok + at the threshold still green (strictly > 2s is yellow).
        assert_eq!(
            classify_health_probe(Ok(std::time::Duration::from_millis(HEALTH_SLOW_MS as u64))),
            tui::HealthKind::Ok
        );
        // Ok + slow → yellow.
        assert_eq!(
            classify_health_probe(Ok(std::time::Duration::from_millis(
                HEALTH_SLOW_MS as u64 + 1
            ))),
            tui::HealthKind::Unstable
        );
        // Transient error → yellow.
        assert_eq!(
            classify_health_probe(Err(anyhow!(
                "upstream returned HTTP 503 Service Unavailable: try later"
            ))),
            tui::HealthKind::Unstable
        );
        assert_eq!(
            classify_health_probe(Err(anyhow!("request failed after retries"))),
            tui::HealthKind::Unstable
        );
        // Permanent 4xx → red.
        assert_eq!(
            classify_health_probe(Err(anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            ))),
            tui::HealthKind::Down
        );
        assert_eq!(
            classify_health_probe(Err(anyhow!(
                "upstream returned HTTP 404 Not Found: no such path"
            ))),
            tui::HealthKind::Down
        );
        // Missing config is handled by the poller as Down (not via this classifier) — here we only
        // assert network-shaped errors.
        let missing = classify_health_probe(Err(anyhow!("no API key — run `aizen config`")));
        assert_eq!(
            missing,
            tui::HealthKind::Unstable,
            "bare 'no API key' has no HTTP code → Transient/yellow; poller maps resolve fail → red"
        );
    }

    #[test]
    fn effort_turn_line_names_the_tier_or_default() {
        // The per-turn status line must contain the tier name (or "default" when the field is
        // omitted), regardless of colour stripping under the test harness.
        assert!(effort_turn_line(Some("high")).contains("high"));
        assert!(effort_turn_line(Some("low")).contains("low"));
        assert!(
            effort_turn_line(Some("xhigh")).contains("xhigh"),
            "xhigh rung named"
        );
        assert!(
            effort_turn_line(Some("max")).contains("max"),
            "max rung named"
        );
        assert!(effort_turn_line(None).contains("default"), "None ⇒ default");
        assert!(
            effort_turn_line(Some("high")).contains("effort:"),
            "always prefixed"
        );
    }

    #[test]
    fn apply_effort_choice_index_mapping_is_total() {
        // Guard the index→tier table the slider feeds apply_effort_choice: 0=auto, 1..=5 pin a tier.
        // (We assert the mapping shape, not the persisted config — save() touches the real config.)
        let tier = |i: usize| ["", "low", "medium", "high", "xhigh", "max"][i];
        assert_eq!(tier(1), "low");
        assert_eq!(tier(2), "medium");
        assert_eq!(tier(3), "high");
        assert_eq!(tier(4), "xhigh");
        assert_eq!(tier(5), "max");
    }

    #[test]
    fn chunk_text_splits_on_utf16_units_not_scalars() {
        // 2100 emoji = 2100 scalars but 4200 UTF-16 units. Under a 3500-unit cap it MUST split —
        // Telegram/Discord count length in UTF-16; naive char-splitting would wrongly keep it whole
        // and the platform would 400 → the reply is silently dropped.
        let s = "🚀".repeat(2100);
        let chunks = chunk_text(&s, 3500);
        assert!(
            chunks.len() >= 2,
            "over-the-UTF16-cap reply must split, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.encode_utf16().count() <= 3500,
                "each chunk within the UTF-16 budget"
            );
        }
        assert_eq!(chunks.concat(), s, "reassembles losslessly");
        assert_eq!(
            chunk_text("hello", 3500),
            vec!["hello".to_string()],
            "ASCII under cap stays whole"
        );

        let rows = "alpha row\nbeta row\ngamma row\n";
        let chunks = chunk_text(rows, 20);
        assert_eq!(chunks.concat(), rows, "line-aware splitting stays lossless");
        assert!(
            chunks[..chunks.len() - 1].iter().all(|c| c.ends_with('\n')),
            "{chunks:?}"
        );
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

    #[test]
    fn ctx_window_matches_known_families() {
        assert_eq!(ctx_window_for("claude-opus-4-8"), 200_000);
        assert_eq!(ctx_window_for("gemini-2.5-pro"), 1_000_000);
        assert_eq!(ctx_window_for("deepseek-chat"), 64_000);
        assert_eq!(ctx_window_for("gpt-4o-mini"), 128_000); // default family
        assert_eq!(ctx_window_for("some-unknown-model"), 128_000); // fallback
    }

    #[test]
    fn classify_source_covers_the_matrix() {
        use super::InstallSource::*;
        // GitHub shorthand
        assert_eq!(
            classify_source("msitarzewski/agency-agents").unwrap(),
            GitHubShorthand("msitarzewski/agency-agents".into())
        );
        // git URLs (https repo, .git, scp-like, ssh)
        assert_eq!(
            classify_source("https://github.com/owner/repo").unwrap(),
            GitUrl("https://github.com/owner/repo".into())
        );
        assert_eq!(
            classify_source("https://github.com/owner/repo.git").unwrap(),
            GitUrl("https://github.com/owner/repo.git".into())
        );
        assert_eq!(
            classify_source("git@github.com:owner/repo.git").unwrap(),
            GitUrl("git@github.com:owner/repo.git".into())
        );
        assert_eq!(
            classify_source("ssh://git@host/owner/repo").unwrap(),
            GitUrl("ssh://git@host/owner/repo".into())
        );
        // single .md file (plain + query-stripped)
        assert_eq!(
            classify_source("https://example.com/a/code-reviewer.md").unwrap(),
            FileUrl("https://example.com/a/code-reviewer.md".into())
        );
        assert_eq!(
            classify_source("https://example.com/x.md?token=abc").unwrap(),
            FileUrl("https://example.com/x.md?token=abc".into())
        );
        // local dir forms
        assert!(matches!(classify_source("./local").unwrap(), LocalDir(_)));
        assert!(matches!(classify_source("/abs/path").unwrap(), LocalDir(_)));
        assert!(matches!(classify_source(".\\win").unwrap(), LocalDir(_)));
        assert!(matches!(
            classify_source("C:\\Users\\me\\agents").unwrap(),
            LocalDir(_)
        ));
        // errors: not a path, not a url, not owner/repo
        assert!(
            classify_source("a/b/c").is_err(),
            "3-segment is not shorthand"
        );
        assert!(classify_source("two words").is_err());
        assert!(classify_source("   ").is_err());
    }

    #[test]
    fn sanitize_repo_name_extracts_clean_dir() {
        assert_eq!(
            sanitize_repo_name("msitarzewski/agency-agents"),
            "agency-agents"
        );
        assert_eq!(
            sanitize_repo_name("https://github.com/owner/repo.git"),
            "repo"
        );
        assert_eq!(sanitize_repo_name("git@github.com:owner/repo.git"), "repo");
        assert_eq!(sanitize_repo_name("/some/local/My Agents"), "My-Agents");
    }

    #[test]
    fn git_url_host_extracts_host_for_ssrf_guard() {
        assert_eq!(
            git_url_host("git@github.com:owner/repo.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("git@10.0.0.5:a/b.git").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            git_url_host("ssh://git@host.example/owner/repo").as_deref(),
            Some("host.example")
        );
        assert_eq!(git_url_host("ssh://host:22/path").as_deref(), Some("host"));
        assert_eq!(
            git_url_host("git://internal/repo").as_deref(),
            Some("internal")
        );
        // http(s) are guarded on the path directly, not via this extractor.
        assert_eq!(git_url_host("https://github.com/o/r"), None);
    }

    #[test]
    fn ctx_bar_uses_semantic_palette() {
        // P-ctx4: colour comes from the semantic palette (OK/WARN/ERR) at the 50%/80% thresholds,
        // not bespoke 256-indices. Force colour on so the ANSI code is actually emitted.
        console::set_colors_enabled(true);
        assert!(
            ctx_bar(30.0).contains(&theme::OK.to_string()),
            "green below 50%"
        );
        assert!(
            ctx_bar(60.0).contains(&theme::WARN.to_string()),
            "gold from 50%"
        );
        assert!(
            ctx_bar(90.0).contains(&theme::ERR.to_string()),
            "salmon from 80%"
        );
    }

    #[test]
    fn ctx_bar_fill_tracks_percentage() {
        // strip ANSI: count the block glyphs.
        let blocks = |pct: f64| ctx_bar(pct).matches('█').count();
        assert_eq!(blocks(0.0), 0);
        assert_eq!(blocks(50.0), 5);
        assert_eq!(blocks(100.0), 10);
        assert_eq!(blocks(150.0), 10); // clamped, never overflows the 10-cell bar
    }

    #[test]
    fn cap_session_drops_oldest_whole_turns_at_user_boundary() {
        // sys + 3 turns (each user + assistant). Cap to 5 → must drop the oldest whole turn(s),
        // keep system[0], and always START the tail at a `user` message.
        let mut h = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        cap_session(&mut h, 5);
        assert!(h.len() <= 5, "trimmed under the cap");
        assert_eq!(h[0].role, "system", "system prompt is preserved");
        assert_eq!(
            h[1].role, "user",
            "tail begins at a user boundary (no orphaned turn)"
        );
        // the most recent turn must survive
        assert!(h.iter().any(|m| m.content.as_deref() == Some("u3")));
    }

    #[test]
    fn cap_session_keeps_single_turn_even_if_over_cap() {
        // One huge turn can't be split at a 2nd user boundary → left intact (loop guard handles size).
        let mut h = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
        ];
        cap_session(&mut h, 2);
        assert_eq!(h.len(), 3, "no safe cut point → keep the turn whole");
    }

    #[test]
    fn dead_end_recovery_detects_error_then_success() {
        let recovered = vec![
            Message::user("do it"),
            Message::assistant("a"),
            Message::tool_result("1", "error: not found"),
            Message::assistant("retry"),
            Message::tool_result("2", "ok, done"),
        ];
        assert!(
            turn_recovered_from_dead_end(&recovered),
            "error then later success = recovery"
        );

        let no_error = vec![
            Message::user("x"),
            Message::tool_result("1", "fine"),
            Message::tool_result("2", "ok"),
        ];
        assert!(
            !turn_recovered_from_dead_end(&no_error),
            "no error → no recovery"
        );

        let only_error = vec![Message::user("x"), Message::tool_result("1", "error: boom")];
        assert!(
            !turn_recovered_from_dead_end(&only_error),
            "error with no later success → no recovery"
        );
    }

    #[test]
    fn compact_cut_lands_on_a_user_boundary() {
        // sys, user, assistant(tool), tool, assistant, user, assistant, user, assistant
        let h = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a-tool"),
            Message::tool_result("id1", "tool-out"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        let cut = agent::compact::plan_compact_cut(&h, COMPACT_KEEP_TURNS).expect("should compact");
        // Tail MUST begin at a user message → never an orphan `tool` result.
        assert_eq!(
            h[cut].role, "user",
            "cut index {cut} is not a user boundary"
        );
        assert!(cut > 1, "must summarize at least one older message");
        // KEEP_TURNS=3, three user turns → keep last 2 → cut at the 2nd user (index 5).
        assert_eq!(cut, 5);
    }

    #[test]
    fn compact_keeps_short_conversations_intact() {
        let k = COMPACT_KEEP_TURNS;
        assert_eq!(
            agent::compact::plan_compact_cut(&[Message::system("s")], k),
            None
        );
        assert_eq!(
            agent::compact::plan_compact_cut(&[Message::system("s"), Message::user("u")], k),
            None
        );
        // one full turn (1 user) → not worth compacting
        assert_eq!(
            agent::compact::plan_compact_cut(
                &[
                    Message::system("s"),
                    Message::user("u"),
                    Message::assistant("a")
                ],
                k
            ),
            None
        );
        // two turns → compact, tail starts at the 2nd user
        let two = vec![
            Message::system("s"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
        ];
        assert_eq!(agent::compact::plan_compact_cut(&two, k), Some(3));
        assert_eq!(two[3].role, "user");
    }

    #[test]
    fn session_names_are_bounded_and_avoid_windows_devices() {
        assert_eq!(sanitize_name("../../chat"), "______chat");
        assert_eq!(sanitize_name("CON"), "session_CON");
        assert_eq!(sanitize_name("com1"), "session_com1");
        assert_eq!(sanitize_name("NUL"), "session_NUL");
        assert_eq!(sanitize_name(""), "session");
        assert!(sanitize_name(&"a".repeat(200)).len() <= 80);
    }

    #[test]
    fn truncate_and_fmt_helpers() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert!(truncate_chars("hello world", 5).starts_with("hello… [+"));
        assert_eq!(fmt_k(300), "300");
        assert_eq!(fmt_k(12_400), "12.4K");
    }

    #[test]
    fn extract_json_object_handles_fences_prose_and_nesting() {
        // fenced + prose around it
        let s = "Sure!\n```json\n{\"worth_saving\": true, \"name\": \"x\"}\n```\ndone";
        let j = extract_json_object(s).unwrap();
        let v: serde_json::Value = serde_json::from_str(j).unwrap();
        assert_eq!(v["worth_saving"], serde_json::json!(true));
        // nested braces + a brace inside a string must not end the object early
        let s2 = r#"{"a": {"b": 1}, "s": "has } brace"}"#;
        assert_eq!(extract_json_object(s2).unwrap(), s2);
        // no object
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn allocate_session_slug_never_reuses_an_existing_file() {
        // Two brand-new chats on the same topic must land on DISTINCT files — the old shared-`last`
        // collision is exactly what made `/sessions` show only the latest conversation.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-slug-alloc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);

        let h = vec![Message::system("s"), Message::user("fix the parser bug")];
        let first = allocate_session_slug(&h);
        // Simulate the first chat having been saved under that slug.
        save_session(&h, &first, None).unwrap();
        let second = allocate_session_slug(&h);
        assert_ne!(
            first, second,
            "second chat on the same topic must not reuse the first's file"
        );
        assert!(!first.is_empty() && !second.is_empty());

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn exit_flush_persists_the_live_conversation_for_sessions() {
        // The whole point of the fix: whatever is live at exit (even a turn the per-turn autosave
        // never reached) is on disk and shows up in /sessions afterwards.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-exit-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        // Start from a clean slug so this chat auto-names a fresh file.
        set_session_slug(None);

        let live = vec![
            Message::system("s"),
            Message::user("remember this across a window close"),
        ];
        update_live_history(&live);
        flush_live_session_on_exit();

        // It must be discoverable and restorable via the same path /sessions uses.
        assert!(
            !scan_sessions().is_empty(),
            "exit flush left nothing in /sessions"
        );
        let slug = current_session_slug().expect("exit flush should have pinned a slug");
        let mut restored = Vec::new();
        let n = load_session(&mut restored, &slug, "opus-4-8").unwrap();
        assert!(
            n >= 1,
            "restored conversation kept its user turn (n counts conversation, not lanes)"
        );
        assert!(
            restored
                .iter()
                .any(|m| m.content.as_deref() == Some("remember this across a window close")),
            "the live user turn survived the exit flush"
        );

        set_session_slug(None);
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
