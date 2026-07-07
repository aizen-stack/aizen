//! `aizen` (alias `ng`) — Aizen first-party agentic coding CLI.
//!
//! Subcommands:
//!   ng chat    — OpenAI-compatible streaming chat (the v0 "call API like hermes" layer)
//!   ng memory  — the standalone, best-for-CLI memory brain (see linked-riding-mochi.md)

// ─── module tree ────────────────────────────────────────────────────────────
// Domains that own a folder: the agent loop, the memory brain, personas, benches.
mod agent;
mod agents; // delegatable specialist sub-agent library (agency-agents format)
mod bench;
mod memory;
mod persona;
// Grouped by role (the src/ reorg — see each folder's mod.rs for what it holds):
mod channels; // telegram · discord · notify
mod core; // types · config · cli_config · net_guard
mod features; // crawl · timemachine · cron · commands
mod llm; // the OpenAI-compatible chat client
mod skills; // skill store + registry
mod ui; // tui · theme · markdown · spinner · splash · icons · image_input

// The reorg moved 23 top-level files into the folders above. These re-exports keep the
// call sites in THIS file referring to the modules by their short names (no behavior
// change) — every other file already uses the new `crate::<group>::<mod>` paths.
use crate::agent::app_catalog;
use crate::channels::{discord, notify, telegram};
use crate::core::{cli_config, config, types};
use crate::features::{commands, crawl, cron, timemachine};
use crate::llm::client;
use crate::persona::soul;
use crate::skills::{self as skill, registry as skill_registry};
use crate::ui::{icons, image_input, splash, theme, tui};

use agent::{AgentConfig, AgentOutcome, StopReason};
use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use console::{style, Style};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
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
    /// route destructive-op approvals to your phone.
    Serve,
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
        /// The key shown by `ng apps list`.
        name: String,
    },
    /// Sign in (OAuth) to a connected remote app — opens your browser (Linear/Notion/Slack/Gmail/…).
    Login {
        /// The key shown by `ng apps list`.
        name: String,
    },
    /// Disconnect an app by its mcp.json key.
    Remove {
        /// The key shown by `ng apps list`.
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
        /// The skill id, e.g. `NousResearch/spike` (from `ng skill search`).
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
    /// Save a checkpoint of the whole working tree (a restore point).
    Save {
        /// Optional label, e.g. `before refactor`.
        label: Vec<String>,
    },
    /// List the timeline (▸ marks the active point).
    List,
    /// Restore the working tree to checkpoint #id (auto-saves the current state first).
    Restore {
        /// Checkpoint id (from `ng time list`).
        id: u32,
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
    /// Delete ALL checkpoints (and free their git objects).
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
        /// Time-machine checkpoints to keep (oldest auto-pruned past this; `0` = unlimited). Default 50.
        #[arg(long)]
        timemachine_keep: Option<usize>,
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
    },
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
    /// Inspect the review queue; --promote <id> accepts one, --clear discards all.
    Review {
        #[arg(long)]
        promote: Option<String>,
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
    /// List archived (LRU-evicted) memories.
    Archive,
    /// Restore an archived memory back into the live store.
    Restore {
        id: String,
    },
    /// Run anti-bloat maintenance (enforce the inferred-fact LRU cap → archive victims).
    Compact,
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
}

#[tokio::main]
async fn main() -> Result<()> {
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
            BenchCmd::Loop => bench::loop_eval::run().await,
        },
        Commands::Config { cmd } => run_config(cmd).await,
        Commands::Models(args) => run_models(args).await,
        Commands::Crawl(args) => run_crawl(args).await,
        Commands::Reach { cmd } => run_reach(cmd).await,
        Commands::Serve => run_serve().await,
        Commands::Telegram { cmd } => run_telegram(cmd).await,
        Commands::Discord { cmd } => run_discord(cmd).await,
        Commands::Time { cmd } => run_time(cmd),
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
                println!("{}", style("✓ trusted — this repo's project MCP servers will load.").color256(splash::ACCENT));
                println!("{}", crate::agent::mcp::summary());
                Ok(())
            }
            McpCmd::Untrust => {
                crate::agent::mcp::untrust_project()?;
                println!("{}", style("project MCP servers untrusted (no longer loaded).").color256(splash::ACCENT));
                Ok(())
            }
        },
        Commands::Apps { cmd } => run_apps(cmd).await,
        Commands::Agents { cmd } => run_agents(cmd).await,
    }
}

/// `ng apps …` — connect apps via the MCP registry.
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
                style(format!("{} result(s) from {} — `aizen apps add <name>` to connect:", hits.len(), app_catalog::registry_base())).dim()
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
                println!("{}", style(format!("✓ disconnected '{name}'.")).color256(splash::ACCENT));
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
    println!("{}", style("Apps — connect via the MCP registry (`aizen apps add <key>`):").bold());
    for f in app_catalog::FEATURED {
        let on = installed.iter().any(|k| k == f.key);
        let badge = if on { style("✓").color256(splash::ACCENT).to_string() } else { style("○").dim().to_string() };
        println!(
            "  {badge}  {} {:<18} {}",
            icons::g(f.icon),
            style(f.key).color256(splash::ACCENT),
            style(f.blurb).dim()
        );
    }
    // Apps the user connected that aren't in the featured set (added via `ng apps add <name>`).
    let custom: Vec<&String> = installed.iter().filter(|k| !app_catalog::FEATURED.iter().any(|f| f.key == **k)).collect();
    if !custom.is_empty() {
        println!("\n{}", style("connected (custom):").bold());
        for k in &custom {
            println!("  {}  {} {}", style("✓").color256(splash::ACCENT), icons::g("🧩"), style(k).color256(splash::ACCENT));
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
        Some(f) => (Some(f.key.to_string()), f.query.to_string(), f.prefer.to_string(), f.label.to_string()),
        None => (None, name.to_string(), name.to_string(), name.to_string()),
    };
    let hits = app_catalog::dedupe_latest(app_catalog::search(&query, 50).await?);
    let viable: Vec<app_catalog::RegistryServer> = hits.into_iter().filter(|s| app_catalog::is_viable(s)).collect();
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
            format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
        }
    };
    let name_w = viable.iter().map(|s| s.short_name().chars().count()).max().unwrap_or(8).clamp(8, 30);
    let tag_w = viable.iter().map(|s| s.transport_tag().chars().count()).max().unwrap_or(7).clamp(7, 14);
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
    let repo = server.repository.as_ref().map(|r| r.url.clone()).unwrap_or_default();
    println!("{}", style(format!("→ {}", server.name)).color256(splash::ACCENT));
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
        let host = server.remotes.get(i).map(|r| app_catalog::host_of(&r.url)).unwrap_or_default();
        println!("  {}", style(format!("⚠ hosted remote @ {host} — a third party runs this server.")).yellow());
        let go = Confirm::with_theme(&theme)
            .with_prompt(format!("Send your credentials to '{host}' (a third party)?"))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !go {
            println!("{}", style("cancelled — no third-party remote connected.").dim());
            return Ok(());
        }
    }
    // OAuth remote → you authenticate directly with the vendor (no token leaves via us); confirm we
    // may open the browser to sign in.
    if let app_catalog::TransportChoice::OAuthRemote(i) = choice {
        let host = server.remotes.get(i).map(|r| app_catalog::host_of(&r.url)).unwrap_or_default();
        println!("  {}", style(format!("🔐 sign-in app @ {host} — Aizen will open your browser to authorize.")).dim());
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
            Password::with_theme(&theme).with_prompt(prompt.trim()).allow_empty_password(true).interact().unwrap_or_default()
        } else {
            Input::<String>::with_theme(&theme).with_prompt(prompt.trim()).allow_empty(true).interact_text().unwrap_or_default()
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
    // the entry is written, the user can retry with `ng apps login <key>`.
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
        style(format!("✓ connected '{key}'.  Its tools load on your next message (/mcp to verify)."))
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
        println!("  {} {}", style("host     ").dim(), style(app_catalog::host_of(url)).dim());
        if entry.get("auth").and_then(|v| v.as_str()) == Some("oauth") {
            let signed = key.map(crate::agent::mcp_oauth::has_token).unwrap_or(false);
            let state = if signed { "signed in".to_string() } else { "not signed in — `aizen apps login <key>`".to_string() };
            println!("  {} oauth ({state})", style("auth     ").dim());
        }
    } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        let args = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        println!("  {} local (stdio)", style("transport").dim());
        println!("  {} {cmd} {args}", style("command  ").dim());
        if !cmd.contains(['/', '\\']) {
            let have = which_runtime(cmd);
            let note = if have { format!("{cmd}: found on PATH") } else { format!("{cmd}: NOT on PATH — install it to run this app") };
            println!("  {} {note}", style("runtime  ").dim());
        }
    }
    for field in ["env", "headers"] {
        if let Some(obj) = entry.get(field).and_then(|v| v.as_object()) {
            for (k, v) in obj {
                println!("  {} {k} = {}", style(format!("{field:<8}")).dim(), mask_secret(v.as_str().unwrap_or("")));
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

/// `ng apps info <key>` — the detail view for ONE connected app: its mcp.json config (transport +
/// secrets MASKED) plus a LIVE probe (handshake + the tools it actually exposes, or why it failed).
async fn apps_info(key: &str) -> Result<()> {
    let Some(entry) = app_catalog::installed_entry(key) else {
        return Err(anyhow!("no connected app keyed '{key}' — see `aizen apps list`"));
    };
    println!("{}", style(key).color256(splash::ACCENT).bold());
    print_entry_summary(&entry, Some(key));

    // Live probe.
    println!("  {}", style("probing (connect + tools/list)…").dim());
    match crate::agent::mcp::probe(key).await {
        Ok(rep) => {
            let info = rep.server_info.get("serverInfo");
            let sname = info.and_then(|s| s.get("name")).and_then(|v| v.as_str()).unwrap_or(key);
            let sver = info.and_then(|s| s.get("version")).and_then(|v| v.as_str()).unwrap_or("");
            println!(
                "  {} {}",
                style("✓").color256(splash::ACCENT),
                style(format!("{sname} {sver}  ·  {} tool(s)", rep.tools.len())).bold()
            );
            for t in &rep.tools {
                let ro = if t.read_only { style(" [read-only]").dim().to_string() } else { String::new() };
                let d: String = t.description.chars().take(72).collect();
                println!("    {}{ro}  {}", style(&t.name).color256(splash::ACCENT), style(d).dim());
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
    let exts: &[&str] = if cfg!(windows) { &["", ".cmd", ".exe", ".bat"] } else { &[""] };
    let Some(path) = std::env::var_os("PATH") else { return false };
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
                println!("{}  {}", f.url, style(format!("[{} d{}]", f.via.tag(), f.depth)).dim());
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
                println!("{}", serde_json::to_string_pretty(&crate::agent::reach::report_json(&reports))?);
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

/// Run the agent loop once (non-streaming, quiet) and return its final text — used by `ng serve`
/// to answer a Telegram message.
async fn run_agent_capture(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    task: &str,
    auto_approve: bool,
) -> Result<String> {
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system = agent::build_top_level_system_prompt(&cwd, std::env::consts::OS, &date, model, Some(&frozen));
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        auto_approve,
        resolve_ctx_window(model).0,
    )?;
    let cfg = AgentConfig { auto_approve, quiet: true, enable_verify_gate: false, ..Default::default() };

    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        client::chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
    };
    let outcome = agent::run_agent(chat, &cfg, &registry, &system, task).await?;
    // A `clarify` yield in a captured (non-REPL) run — e.g. `ng serve` — has no input box to loop
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
    while history.len() > max {
        // index of the SECOND user message (the start of the 2nd turn); drop [1, that).
        let second_user =
            history.iter().enumerate().filter(|(i, m)| *i >= 1 && m.role == "user").nth(1).map(|(i, _)| i);
        match second_user {
            Some(i) => {
                history.drain(1..i);
            }
            None => break, // only one turn present → nothing safe to drop; the loop guard handles it
        }
    }
}

/// Run one `ng serve` turn over a PERSISTENT per-chat history, so follow-ups like "now fix it" keep
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
    auto_approve: bool,
) -> Result<String> {
    if history.is_empty() {
        // Built once per session → the prefix stays byte-stable across the conversation (cache-warm).
        history.push(Message::system(current_system_prompt(model)));
    }
    history.push(Message::user(task.to_string()));

    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        auto_approve,
        resolve_ctx_window(model).0,
    )?;
    let cfg = AgentConfig {
        auto_approve,
        quiet: true,
        enable_verify_gate: false,
        context_window: resolve_ctx_window(model).0,
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

    // Memory is the moat — passively learn durable facts from this turn (free; core stays gated).
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

/// `ng serve` — the long-lived daemon: one poll loop owns getUpdates, an agent runner handles one
/// message at a time, and destructive-op approvals route to the phone (via the approval gate).
async fn run_serve() -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (client, cfg) = telegram::configured().context("telegram not configured — run `aizen telegram setup` first")?;
    let (base_url, api_key, model) =
        resolve_endpoint(None, None, None).context("configure the model endpoint first (run `aizen config`)")?;
    let http = http_client()?;

    telegram::set_daemon_active(true);
    let client = Arc::new(client);
    eprintln!("{}", style(format!("aizen serve — listening on Telegram (Ctrl-C to stop). chats: {:?}", cfg.allowed_chat_ids)).dim());

    let (tx, mut rx) = mpsc::channel::<(i64, String)>(64);

    let poll_client = client.clone();
    let poll_cfg = cfg.clone();
    let poll = tokio::spawn(async move {
        let mut offset = 0i64;
        loop {
            let updates = match poll_client.get_updates(offset, telegram::POLL_TIMEOUT_SECS).await {
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
                        if let Some((id, ok)) = cb.data.as_deref().and_then(telegram::parse_callback) {
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
    let mut sessions: std::collections::HashMap<i64, Vec<Message>> = std::collections::HashMap::new();

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
            let _ = client.send_message(chat, "🆕 started a fresh conversation — earlier context dropped.").await;
            continue;
        }
        if trimmed == "/resume" {
            let turns = sessions.get(&chat).map(|h| h.iter().filter(|m| m.role == "user").count()).unwrap_or(0);
            let msg = if turns == 0 {
                "🧵 no active conversation — just send a message to start one.".to_string()
            } else {
                format!("🧵 continuing — {turns} message(s) of context kept. /new to start over.")
            };
            let _ = client.send_message(chat, &msg).await;
            continue;
        }
        let (task, auto) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), true),
            None => (trimmed.to_string(), false),
        };
        if task.is_empty() {
            continue;
        }
        let _ = client.send_message(chat, "⏳ working…").await;
        let history = sessions.entry(chat).or_default();
        let reply = run_serve_turn(&http, &base_url, &api_key, &model, history, &task, auto)
            .await
            .unwrap_or_else(|e| format!("error: {e}"));
        for piece in chunk_text(&reply, 3500) {
            let _ = client.send_message(chat, &piece).await;
        }
    }

    telegram::set_daemon_active(false);
    poll.abort();
    Ok(())
}

// ───────────────────────────── time machine (git snapshots) ─────────────────────────────

fn run_time(cmd: TimeCmd) -> Result<()> {
    match cmd {
        TimeCmd::Save { label } => {
            let snap = timemachine::save(&label.join(" "), false)?;
            println!("{} #{}  {}", style("✓ checkpoint").color256(splash::ACCENT), snap.id, style(&snap.created).dim());
            Ok(())
        }
        TimeCmd::List => print_timeline(),
        TimeCmd::Restore { id } => {
            let snap = timemachine::restore(id)?;
            let label = if snap.label.is_empty() { "(no label)".to_string() } else { snap.label.clone() };
            println!("{} #{} — {label}", style("⏪ restored to").color256(splash::ACCENT), snap.id);
            // Say WHAT changed and that it's undoable: aizen only rewinds the working tree (files),
            // never your chat/history — and because the pre-restore state was auto-snapshotted, you
            // can always go forward again (`aizen time redo`, or restore the newest checkpoint).
            println!("{}", style("  files only — your conversation is untouched · reversible with `aizen time redo`").dim());
            Ok(())
        }
        TimeCmd::Undo => {
            let snap = timemachine::undo()?;
            println!("{} #{}", style("⏪ undo →").color256(splash::ACCENT), snap.id);
            Ok(())
        }
        TimeCmd::Redo => {
            let snap = timemachine::redo()?;
            println!("{} #{}", style("⏩ redo →").color256(splash::ACCENT), snap.id);
            Ok(())
        }
        TimeCmd::Prune { keep } => {
            let k = keep.or(cli_config::load().timemachine_keep).unwrap_or(50);
            let dropped = timemachine::prune(k)?;
            println!("{} {dropped} old checkpoint(s); kept ≤{k}.", style("🧹 pruned").color256(splash::ACCENT));
            Ok(())
        }
        TimeCmd::Clear => {
            let n = timemachine::clear()?;
            println!("{} {n} checkpoint(s) deleted.", style("🧹 cleared").color256(splash::ACCENT));
            Ok(())
        }
    }
}

/// Print the snapshot timeline (▸ = the active point).
fn print_timeline() -> Result<()> {
    let (snaps, cursor) = timemachine::timeline()?;
    if snaps.is_empty() {
        println!("(no checkpoints yet — `aizen time save [label]`, or /checkpoint in the REPL)");
        return Ok(());
    }
    for (i, s) in snaps.iter().enumerate() {
        let here = if Some(i) == cursor {
            style("▸").color256(splash::ACCENT).bold().to_string()
        } else {
            " ".to_string()
        };
        let label = if s.label.is_empty() { "(no label)".to_string() } else { s.label.clone() };
        let tag = if s.auto { style(" · auto").dim().to_string() } else { String::new() };
        println!("{here} #{:<3} {}  {label}{tag}", s.id, style(&s.created).dim());
    }
    let keep = cli_config::load().timemachine_keep.unwrap_or(50);
    let limit = if keep == 0 { "unlimited".to_string() } else { format!("auto-prune oldest past {keep}") };
    println!("{}", style(format!("{} checkpoint(s) · {limit} · `aizen time prune`/`clear` to tidy", snaps.len())).dim());
    Ok(())
}

/// `/timeline` — interactive time machine: pick a checkpoint to restore, or save a new one.
///
/// Restore offers up to three tiers (Cline-style), depending on whether the checkpoint captured the
/// conversation too:
///   • Restore Files — rewind only the working tree; the chat stays (always available).
///   • Restore Task Only — rewind only the conversation to where it was then; files stay.
///   • Restore Files & Task — rewind both.
/// A checkpoint saved through `/timeline` / `/checkpoint` carries a chat sidecar (all three tiers);
/// an auto/agent checkpoint is Files-only. Every restore is reversible — the pre-restore tree is
/// auto-snapshotted, and the current chat is saved to the `last` session before a task rewind.
async fn timeline_menu(history: &mut Vec<Message>, model_label: &mut String) -> Result<()> {
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
                let label = if s.label.is_empty() { "(no label)".to_string() } else { s.label.clone() };
                let tag = if s.auto { " · auto" } else { "" };
                let chat = if s.has_chat { " · +chat" } else { "" };
                format!("{here}#{} {}  {label}{tag}{chat}", s.id, style(&s.created).dim())
            })
            .collect();
        items.push("✚ Save a checkpoint now (code + chat)".to_string());
        items.push("Back".to_string());
        let prompt = format!(
            "Time machine — {n} checkpoint(s); pick one to restore (reversible). Esc to go back"
        );
        let pick = match Select::with_theme(&theme).with_prompt(prompt).items(&items).default(cursor.unwrap_or(0)).interact_opt()? {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            restore_menu(&snaps[pick], history, model_label, &theme)?;
        } else if pick == n {
            let label: String =
                Input::with_theme(&theme).with_prompt("Label (optional)").allow_empty(true).interact_text()?;
            // Capture the conversation alongside the tree so this checkpoint supports task restore.
            match timemachine::save_with_chat(label.trim(), false, history) {
                Ok(s) => println!("{} #{} (code + chat)", style("✓ checkpoint").color256(splash::ACCENT), s.id),
                Err(e) => println!("{}", style(format!("save failed: {e}")).red()),
            }
        } else {
            return Ok(());
        }
    }
}

/// The per-checkpoint restore sub-menu: Files / Task / Both, gated on whether a chat sidecar exists.
fn restore_menu(
    snap: &timemachine::Snapshot,
    history: &mut Vec<Message>,
    model_label: &mut String,
    theme: &dialoguer::theme::ColorfulTheme,
) -> Result<()> {
    // Files-only checkpoints (auto/agent) can't restore the chat — do the files rewind directly.
    if !snap.has_chat {
        return files_restore(snap.id);
    }
    let opts = [
        "Restore Files (code only — chat stays)",
        "Restore Task Only (chat only — files stay)",
        "Restore Files & Task (both)",
        "Cancel",
    ];
    let pick = match Select::with_theme(theme)
        .with_prompt(format!("Restore checkpoint #{} — what to rewind?", snap.id))
        .items(&opts)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    match pick {
        0 => files_restore(snap.id)?,
        1 => task_restore(snap.id, history, model_label)?,
        2 => {
            files_restore(snap.id)?;
            task_restore(snap.id, history, model_label)?;
        }
        _ => {}
    }
    Ok(())
}

/// Rewind only the working tree to checkpoint `id` (reversible — pre-restore tree auto-saved).
fn files_restore(id: u32) -> Result<()> {
    match timemachine::restore(id) {
        Ok(s) => {
            println!("{} #{} — files rewound; your chat is untouched", style("⏪ restored").color256(splash::ACCENT), s.id);
            println!("{}", style("  (reversible — the pre-restore tree was auto-saved; pick it to go back)").dim());
        }
        Err(e) => println!("{}", style(format!("restore failed: {e}")).red()),
    }
    Ok(())
}

/// Rewind only the conversation to the sidecar captured with checkpoint `id`; files stay as they are.
/// Before overwriting, the CURRENT chat is saved to the `last` session so it's never lost.
fn task_restore(id: u32, history: &mut Vec<Message>, model_label: &mut String) -> Result<()> {
    match timemachine::load_chat(id) {
        Some(chat) if !chat.is_empty() => {
            let _ = save_session(history, "last"); // don't lose the current chat — it's recoverable via /sessions
            *history = chat;
            // The restored transcript carries its own system prompt at [0]; refresh the model label
            // line so the HUD matches, without rebuilding (which would wipe the restored history).
            let _ = model_label; // label is display-only; the restored chat already holds its system prompt
            println!("{} #{} — conversation rewound; files untouched", style("⏪ restored task").color256(splash::ACCENT), id);
            println!("{}", style("  (your previous chat was saved as `last` — /sessions to get it back)").dim());
        }
        _ => println!("{}", style(format!("checkpoint #{id} has no saved conversation to restore")).color256(crate::ui::theme::WARN).to_string()),
    }
    Ok(())
}

// ───────────────────────────── discord bot daemon + setup ─────────────────────────────

async fn run_discord(cmd: DiscordCmd) -> Result<()> {
    match cmd {
        DiscordCmd::Setup => discord_setup().await,
        DiscordCmd::Test => discord_test().await,
        DiscordCmd::Serve => run_discord_serve().await,
        DiscordCmd::Show => {
            discord_status();
            Ok(())
        }
        DiscordCmd::Disable => discord_disable(),
    }
}

async fn discord_test() -> Result<()> {
    let (client, _) = discord::configured().context("Discord bot not set up — run `aizen discord setup`")?;
    let name = client.get_me().await?;
    println!("{}", style(format!("✓ bot token valid — @{name}")).color256(splash::ACCENT));
    Ok(())
}

fn discord_status() {
    let d = cli_config::load().discord.unwrap_or_default();
    let token = d.resolved_token().map(|t| cli_config::mask(&t)).unwrap_or_else(|| "not set".to_string());
    println!("{}", style("Discord bot").bold().color256(splash::ACCENT));
    println!("token:    {token}");
    println!("channels: {:?}", d.allowed_channel_ids);
    if !d.allowed_user_ids.is_empty() {
        println!("users:    {:?}", d.allowed_user_ids);
    }
    println!("configured: {}", if discord::is_configured() { "yes" } else { "no" });
}

fn discord_disable() -> Result<()> {
    let mut cfg = cli_config::load();
    if cfg.discord.is_none() {
        println!("(Discord bot was not configured)");
        return Ok(());
    }
    cfg.discord = None;
    cli_config::save(&cfg)?;
    println!("{}", style("Discord bot disabled (config removed).").color256(splash::ACCENT));
    Ok(())
}

/// Interactive Discord setup: paste the bot token (validated via /users/@me), then the channel id(s)
/// the bot may respond in.
async fn discord_setup() -> Result<()> {
    let theme = ui_theme();
    println!("\n{}", style("Discord bot setup").bold().color256(splash::ACCENT));
    println!(
        "{}",
        style("Create an app + bot at discord.com/developers, ENABLE the \"Message Content Intent\", invite \
               it to your server, copy the bot token.")
            .dim()
    );

    let mut cfg = cli_config::load();
    let mut d = cfg.discord.clone().unwrap_or_default();
    let cur = d.token.as_deref().map(cli_config::mask).unwrap_or_else(|| "none".to_string());
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
    let name = client.get_me().await.context("Discord rejected the token — check it and retry")?;
    println!("{}", style(format!("✓ bot @{name}")).color256(splash::ACCENT));

    let cur_ch = if d.allowed_channel_ids.is_empty() {
        String::new()
    } else {
        d.allowed_channel_ids.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
    };
    let chans: String = Input::with_theme(&theme)
        .with_prompt("Allowed channel id(s), comma-separated (right-click a channel → Copy Channel ID)")
        .with_initial_text(cur_ch)
        .allow_empty(true)
        .interact_text()
        .context("reading channel ids")?;
    let ids: Vec<u64> = chans.split(',').filter_map(|s| s.trim().parse::<u64>().ok()).collect();
    if !ids.is_empty() {
        d.allowed_channel_ids = ids;
    }
    if d.allowed_channel_ids.is_empty() {
        anyhow::bail!("at least one allowed channel id is required (the bot is deny-by-default)");
    }

    cfg.discord = Some(d);
    cli_config::save(&cfg)?;
    println!("\n{}", style("Saved. Start the bot with:  aizen discord serve").color256(splash::ACCENT));
    Ok(())
}

/// `ng discord serve` — the Discord bot daemon. A gateway task receives messages (heartbeating
/// independently); this loop runs the agent one message at a time (per-channel history) and replies
/// over REST. Mirrors `run_serve` (Telegram). NOTE: destructive-op approvals are not yet routed to
/// Discord, so edits need `/yolo`/smart approval; read/research work as-is.
async fn run_discord_serve() -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (client, cfg) = discord::configured().context("Discord bot not configured — run `aizen discord setup`")?;
    let (base_url, api_key, model) =
        resolve_endpoint(None, None, None).context("configure the model endpoint first (run `aizen config`)")?;
    let http = http_client()?;
    let token = cfg.resolved_token().context("no bot token")?;
    let client = Arc::new(client);
    eprintln!(
        "{}",
        style(format!("aizen serve — listening on Discord (Ctrl-C to stop). channels: {:?}", cfg.allowed_channel_ids)).dim()
    );

    let (tx, mut rx) = mpsc::channel::<discord::Incoming>(64);
    let gw_cfg = cfg.clone();
    let gw = tokio::spawn(async move { discord::run_gateway(token, gw_cfg, tx).await });

    // Per-channel conversation history → follow-ups keep context (in-memory; /new resets).
    let mut sessions: std::collections::HashMap<u64, Vec<Message>> = std::collections::HashMap::new();
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
            let _ = client.send_message(inc.channel_id, "🆕 started a fresh conversation — earlier context dropped.").await;
            continue;
        }
        let (task, auto) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), true),
            None => (trimmed.to_string(), false),
        };
        if task.is_empty() {
            continue;
        }
        let _ = client.send_message(inc.channel_id, "⏳ working…").await;
        let history = sessions.entry(inc.channel_id).or_default();
        let reply = run_serve_turn(&http, &base_url, &api_key, &model, history, &task, auto)
            .await
            .unwrap_or_else(|e| format!("error: {e}"));
        for piece in chunk_text(&reply, discord::MESSAGE_MAX) {
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
    let (client, cfg) = telegram::configured().context("Telegram not set up — choose Set up first")?;
    let chat = telegram::first_chat(&cfg).context("no allowed chat id — re-run Set up")?;
    client.send_message(chat, "✅ Aizen test message — Telegram is wired up.").await?;
    println!("{}", style(format!("sent a test message to chat {chat}")).color256(splash::ACCENT));
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
    println!("daemon:   {}", if telegram::daemon_is_active() { "running (this process)" } else { "stopped" });
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
    println!("{}", style("Telegram disabled (bot config removed).").color256(splash::ACCENT));
    Ok(())
}

/// A NextGen "connected app" surfaced in the `/apps` hub. Telegram is two-way (a long-poll daemon +
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
            Integration::Discord => discord::is_configured() || notify::is_configured(notify::Channel::Discord),
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

    // Rows: featured apps first, then any connected custom apps (added via `ng apps add <name>`).
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
            rows.push(Row { key: k.clone(), label: k.clone(), icon: "🧩".to_string(), connected: true, featured: false });
        }
    }

    let mut items: Vec<String> = rows
        .iter()
        .map(|r| {
            let badge = if r.connected { style("✓").color256(splash::ACCENT).to_string() } else { style("○").dim().to_string() };
            let blurb = if r.featured {
                app_catalog::featured(&r.key).map(|f| f.blurb).unwrap_or("")
            } else {
                "connected (custom)"
            };
            let action = if r.connected { style("manage").color256(splash::ACCENT).to_string() } else { style(blurb).dim().to_string() };
            format!("{badge}  {} {}  —  {}", icons::g(r.icon.as_str()), r.label, action)
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
        let q: String = Input::with_theme(&theme).with_prompt("Search the MCP registry for").allow_empty(true).interact_text()?;
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
    let is_oauth =
        app_catalog::installed_entry(key).and_then(|e| e.get("auth").and_then(|v| v.as_str()).map(|s| s == "oauth")).unwrap_or(false);
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
            println!("{}", style(format!("testing '{key}' (connect + tools/list)…")).dim());
            match crate::agent::mcp::probe(key).await {
                Ok(rep) => println!(
                    "{}",
                    style(format!("✓ '{key}' connected — {} tool(s) available.", rep.tools.len())).color256(splash::ACCENT)
                ),
                Err(e) => println!("{}", style(format!("✗ '{key}' failed — {e:#}")).red()),
            }
            Ok(())
        }
        "Sign in again (OAuth)" => {
            match crate::agent::mcp::login(key).await {
                Ok(()) => println!("{}", style(format!("✓ signed in to '{key}'. Takes effect on your next message.")).color256(splash::ACCENT)),
                Err(e) => println!("{}", style(format!("✗ sign-in failed — {e:#}")).red()),
            }
            Ok(())
        }
        "Disconnect" => {
            let yes = Confirm::with_theme(&theme).with_prompt(format!("Disconnect '{key}'?")).default(false).interact()?;
            if yes {
                if app_catalog::remove_server(key)? {
                    crate::agent::mcp_oauth::clear_token(key); // drop any cached OAuth token too
                    crate::agent::mcp::invalidate();
                    println!("{}", style(format!("✓ disconnected '{key}'. Takes effect on your next message.")).color256(splash::ACCENT));
                } else {
                    println!("{}", style(format!("'{key}' was not present.")).dim());
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `/apps → Discord` — Discord can be a two-way BOT (receives + replies, needs `ng discord serve`)
/// and/or a one-way notify WEBHOOK (fire-and-forget alerts). One menu offers both.
async fn discord_app_menu() -> Result<()> {
    let theme = ui_theme();
    let bot = if discord::is_configured() { "bot ✓" } else { "bot ○" };
    let hook = if notify::is_configured(notify::Channel::Discord) { "webhook ✓" } else { "webhook ○" };
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
        2 => run_discord_serve().await,
        3 => webhook_app_setup(notify::Channel::Discord).await,
        4 => discord_disable(),
        _ => Ok(()),
    }
}

/// `/apps` — the integrations hub: NextGen's connected apps (Telegram today; Discord/Slack/webhooks
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
            format!("{badge}  {}{}  —  {}", icons::g(i.icon()), i.name(), style(i.blurb()).dim())
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
    let status = if configured { "configured" } else { "not set up" };
    let items: Vec<&str> = if configured {
        vec!["Set / update URL", "Send a test notification", "Disable  (remove the URL)", "Back"]
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
    println!("\n{}", style(format!("{} setup", ch.label())).bold().color256(splash::ACCENT));
    println!("{}", style(ch.setup_hint()).dim());

    let mut cfg = cli_config::load();
    let mut n = cfg.notify.clone().unwrap_or_default();
    let cur = notify::channel_url(ch, &cfg).map(|u| cli_config::mask(&u)).unwrap_or_else(|| "none".to_string());
    let entered: String = Input::with_theme(&theme)
        .with_prompt(format!("{} URL (current {cur} — Enter to keep)", ch.label()))
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
        let cur_auth = n.webhook_auth.as_deref().map(cli_config::mask).unwrap_or_else(|| "none".to_string());
        let auth: String = Input::with_theme(&theme)
            .with_prompt(format!("Auth header — e.g. 'Authorization: Bearer …' (current {cur_auth} — Enter to skip)"))
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
        match notify::send_to(ch, "✅ Aizen connected — this channel will receive agent notifications.").await {
            Ok(()) => println!("{}", style(format!("✓ test delivered to {}", ch.label())).color256(splash::ACCENT)),
            Err(e) => println!("{}", style(format!("✗ test failed: {e}")).red()),
        }
    }
    Ok(())
}

/// Send a one-off test notification to a configured outbound app.
async fn webhook_app_test(ch: notify::Channel) -> Result<()> {
    println!("{}", style(format!("Sending a test notification to {}…", ch.label())).dim());
    match notify::send_to(ch, "🔔 Aizen test notification.").await {
        Ok(()) => println!("{}", style(format!("✓ delivered to {}", ch.label())).color256(splash::ACCENT)),
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
    println!("{}", style(format!("{} disabled (URL removed).", ch.label())).color256(splash::ACCENT));
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
    let name: String = Input::with_theme(&theme).with_prompt("Skill name").interact_text()?;
    if name.trim().is_empty() {
        anyhow::bail!("a skill name is required");
    }
    let description: String =
        Input::with_theme(&theme).with_prompt("Description (one line)").allow_empty(true).interact_text()?;
    let when: String = Input::with_theme(&theme)
        .with_prompt("When does it apply? (trigger hint)")
        .allow_empty(true)
        .interact_text()?;
    println!("{}", style("Steps — type the procedure; end with a line containing only '.'").dim());
    let body = read_multiline_until_dot()?;
    if body.trim().is_empty() {
        anyhow::bail!("the steps are required");
    }
    let path = skill::save(&name, &description, &when, &body)?;
    println!("{}", style(format!("saved skill → {}", path.display())).color256(splash::ACCENT));
    Ok(())
}

/// Prompt for a URL and fetch a skill from it.
async fn skill_fetch_interactive() -> Result<()> {
    let theme = ui_theme();
    let url: String =
        Input::with_theme(&theme).with_prompt("Skill URL (raw markdown, e.g. a gist/raw GitHub link)").interact_text()?;
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
            Ok(true) => println!("{}", style(format!("deleted '{}'", names[i])).color256(splash::ACCENT)),
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
                let d = if s.description.is_empty() { s.when.clone() } else { s.description.clone() };
                format!("{}  —  {}", s.name, style(d).dim())
            })
            .collect();
        items.push("+ New skill".to_string());
        items.push("⬇ Fetch from URL".to_string());
        items.push(format!("🔎 Search agentskill.sh  {}", style("(marketplace)").dim()));
        if n > 0 {
            items.push("✗ Delete a skill".to_string());
        }
        items.push("Back".to_string());
        let prompt = format!("Skills — {n} saved (Esc to go back)");
        let pick = match Select::with_theme(&theme).with_prompt(prompt).items(&items).default(0).interact_opt()? {
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
        .with_prompt(format!("Search {} for a skill", skill_registry::registry_base()))
        .interact_text()
        .context("reading query")?;
    if query.trim().is_empty() {
        return Ok(());
    }
    println!("{}", style("Searching…").dim());
    let hits = skill_registry::search(query.trim(), 20).await?;
    if hits.is_empty() {
        println!("{}", style(format!("no skills match '{}'", query.trim())).dim());
        return Ok(());
    }
    let mut items: Vec<String> =
        hits.iter().map(|s| format!("{}  {}", s.id(), style(s.summary_line().splitn(2, " — ").nth(1).unwrap_or("")).dim())).collect();
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
    println!("{} '{}'.", style("✓ installed").color256(splash::ACCENT), sk.name);
    Ok(())
}

/// Author a new persona interactively (name + role + voice + multi-line description).
fn persona_new_interactive() -> Result<()> {
    let theme = ui_theme();
    let name: String = Input::with_theme(&theme).with_prompt("Persona name (e.g. Aria)").interact_text()?;
    if name.trim().is_empty() {
        anyhow::bail!("a persona name is required");
    }
    let role: String =
        Input::with_theme(&theme).with_prompt("Role (one line, e.g. a sharp senior-engineer mentor)").allow_empty(true).interact_text()?;
    let voice: String = Input::with_theme(&theme)
        .with_prompt("Voice (e.g. concise, warm, a little sardonic)")
        .allow_empty(true)
        .interact_text()?;
    println!("{}", style("Backstory / values / how it behaves — end with a line containing only '.'").dim());
    let body = read_multiline_until_dot()?;
    if body.trim().is_empty() {
        anyhow::bail!("a description is required");
    }
    let path = persona::save(&name, &role, &voice, &body)?;
    println!("{}", style(format!("saved persona → {}", path.display())).color256(splash::ACCENT));
    Ok(())
}

/// Paste a raw character / system prompt and have the model distill it into a persona card
/// (name + role + voice + a rewritten body). Then offer to activate it for the current chat.
async fn persona_paste_interactive(history: &mut Vec<Message>, model: &str) -> Result<()> {
    let theme = ui_theme();
    println!(
        "{}",
        style("Paste the character / system prompt below — end with a line containing only '.'").dim()
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
    let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    let role = v.get("role").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    let voice = v.get("voice").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    let body = v.get("body").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    let name = if name.is_empty() { "Character".to_string() } else { name };
    let body = if body.is_empty() { pasted.trim().to_string() } else { body };

    let path = persona::save(&name, &role, &voice, &body)?;
    println!(
        "{}",
        style(format!("created persona '{name}' → {}", path.display())).color256(splash::ACCENT)
    );
    println!("  {} {}", style("role:").dim(), if role.is_empty() { "(none)".into() } else { role });
    println!("  {} {}", style("voice:").dim(), if voice.is_empty() { "(none)".into() } else { voice });

    if Confirm::with_theme(&theme).with_prompt(format!("Play as {name} now?")).default(true).interact()? {
        let mut cfg = cli_config::load();
        cfg.persona = Some(name.clone());
        cli_config::save(&cfg)?;
        update_system_prompt(history, model);
        println!("{}", style(format!("now playing: {name}")).color256(splash::ACCENT));
    }
    Ok(())
}

/// Show a character's accumulated self-memory (reflected insights + recent episodes).
fn persona_self_view(slug: &str, name: &str) {
    let mut mems = persona::self_mem::list(slug);
    if mems.is_empty() {
        println!("{}", style(format!("{name} has no self-memory yet — it grows as you chat.")).dim());
        return;
    }
    let (eps, ins) = persona::self_mem::counts(slug);
    println!(
        "{}",
        style(format!("{name} — {ins} insight(s), {eps} episode(s)")).color256(splash::ACCENT).bold()
    );
    if persona::self_mem::should_reflect(slug) {
        println!(
            "{}",
            style("  → primed to reflect: the next turn synthesizes recent episodes into insights").dim()
        );
    }
    // insights first (the durable layer), newest first
    mems.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    let insights: Vec<&persona::self_mem::SelfMemory> =
        mems.iter().filter(|m| m.kind == persona::self_mem::Kind::Insight).collect();
    if !insights.is_empty() {
        println!("\n{}", style("insights").dim());
        for m in insights.iter().take(10) {
            println!("  {} [{}] {}", style("★").color256(splash::ACCENT), m.importance, truncate_chars(m.body.trim(), 140));
        }
    }
    let episodes: Vec<&persona::self_mem::SelfMemory> =
        mems.iter().filter(|m| m.kind == persona::self_mem::Kind::Episode).collect();
    if !episodes.is_empty() {
        println!("\n{}", style("recent episodes").dim());
        for m in episodes.iter().take(8) {
            println!("  {} [{}] {}", style("·").dim(), m.importance, truncate_chars(m.body.trim(), 120));
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
                let sub = if p.role.is_empty() { p.voice.clone() } else { p.role.clone() };
                format!("{badge}  {}{}  —  {}", icons::g(icons::slash("persona")), p.name, style(sub).dim())
            })
            .collect();
        // actions after the persona list
        let active_slug_self = active_slug.clone();
        let (n_eps, n_ins) =
            active_slug_self.as_deref().map(persona::self_mem::counts).unwrap_or((0, 0));
        let has_self = n_eps + n_ins > 0;
        let mut actions: Vec<String> =
            vec!["+ New persona".to_string(), "Paste a character prompt → auto-create".to_string()];
        if active.is_some() {
            actions.push(format!(
                "Evolution: {} (toggle)",
                if persona_evolve_enabled() { "ON" } else { "OFF" }
            ));
        }
        if has_self {
            actions.push(format!("View self-memory ({n_ins} insights, {n_eps} episodes)"));
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
        let pick = match Select::with_theme(&theme).with_prompt(prompt).items(&items).default(0).interact_opt()? {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            // select this persona
            let mut cfg = cli_config::load();
            cfg.persona = Some(personas[pick].name.clone());
            cli_config::save(&cfg)?;
            update_system_prompt(history, model);
            println!("{}", style(format!("now playing: {}", personas[pick].name)).color256(splash::ACCENT));
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
                    style(format!("persona evolution {}", if now { "ON" } else { "OFF" }))
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
                        style(format!("reset self-memory ({n} item(s) cleared)")).color256(splash::ACCENT)
                    );
                }
            }
            "Use default voice (no persona)" => {
                let mut cfg = cli_config::load();
                cfg.persona = None;
                cli_config::save(&cfg)?;
                update_system_prompt(history, model);
                println!("{}", style("persona cleared → default assistant voice").color256(splash::ACCENT));
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
                        Ok(true) => println!("{}", style(format!("deleted '{}'", names[i])).color256(splash::ACCENT)),
                        Ok(false) => println!("{}", style("(already gone)").dim()),
                        Err(e) => eprintln!("{} {e}", style("persona:").red()),
                    }
                }
            }
            _ => return Ok(()), // Back
        }
    }
}

/// `/telegram` — a dedicated sub-menu for the Telegram integration (one of NextGen's connected
/// apps): set up, test, status, start the phone-control daemon, or disable.
async fn telegram_menu() -> Result<()> {
    let theme = ui_theme();
    let configured = telegram::is_configured();
    let status = if configured { "configured" } else { "not set up" };
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
        3 => run_serve().await,
        4 => telegram_disable(),
        _ => Ok(()),
    }
}

/// Interactive Telegram setup: paste the @BotFather token, validate via getMe, then capture the
/// owner's chat id from the first message they send the bot.
async fn telegram_setup() -> Result<()> {
    let theme = ui_theme();
    println!("\n{}", style("Telegram setup").bold().color256(splash::ACCENT));
    println!("{}", style("Create a bot with @BotFather (/newbot), copy the token it gives you.").dim());

    let mut cfg = cli_config::load();
    let mut tg = cfg.telegram.clone().unwrap_or_default();
    let cur = tg.token.as_deref().map(cli_config::mask).unwrap_or_else(|| "none".to_string());
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
    let username = client.get_me().await.context("Telegram rejected the token — check it and retry")?;
    println!("{}", style(format!("✓ bot @{username}")).color256(splash::ACCENT));

    println!(
        "{}",
        style(format!("Now open Telegram → find @{username} → send it any message. Waiting (≤120s)…")).dim()
    );
    let chat = poll_for_chat_id(&client).await?;
    if !tg.allowed_chat_ids.contains(&chat) {
        tg.allowed_chat_ids.push(chat);
    }
    println!("{}", style(format!("✓ captured chat id {chat}")).color256(splash::ACCENT));

    cfg.telegram = Some(tg);
    cli_config::save(&cfg)?;
    let _ = client.send_message(chat, "✅ Aizen connected. Run `aizen serve`, then send /help.").await;
    println!("\n{}", style("Saved. Start the daemon with:  aizen serve").color256(splash::ACCENT));
    Ok(())
}

/// Long-poll until the owner sends the bot a message; return that chat id (≤120s, else error).
async fn poll_for_chat_id(client: &telegram::Client) -> Result<i64> {
    let start = tokio::time::Instant::now();
    let mut offset = 0i64;
    while start.elapsed() < std::time::Duration::from_secs(120) {
        let updates = client.get_updates(offset, 20).await.context("polling for your message")?;
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
        active_item_prefix: style("❯".to_string()).for_stderr().color256(splash::ACCENT).bold(),
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
    let bar = |l: &str, r: &str| style(format!("{l}{}{r}", "─".repeat(W))).color256(accent).to_string();
    // A small status tag in the TOP border (`╭───────[1 img]─╮`). ASCII-only + right-aligned, so the
    // width is exact and the border never tears (an emoji caption mis-measures by a cell). Empty tag
    // → a plain border.
    let top_bar = |tag: &str| -> String {
        if tag.is_empty() {
            return bar("╭", "╮");
        }
        let t = format!("[{tag}]");
        let fill = W.saturating_sub(t.chars().count() + 1);
        style(format!("╭{}{t}─╮", "─".repeat(fill))).color256(accent).to_string()
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
                    println!("{} {}", style("❯").color256(accent).bold(), style(trimmed).dim());
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
        println!("Run `aizen --help` for commands, or `aizen config` to set up the endpoint + key.");
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
/// + load them, or dismiss (won't nag again — `ng mcp trust` re-enables). MCP servers can run
/// commands, hence the explicit gate before auto-arming a stranger's repo.
fn prompt_mcp_trust(server_count: usize) {
    let theme = ui_theme();
    println!(
        "\n{}",
        style(format!("⚠ This repo ships {server_count} MCP tool server(s) (./.aizen/mcp.json).")).color256(crate::ui::theme::WARN)
    );
    println!("{}", style("MCP servers can run commands on your machine — only trust repos you trust.").color256(crate::ui::theme::FAINT));
    let ok = Confirm::with_theme(&theme)
        .with_prompt("Trust this repo and load its MCP servers?")
        .default(false)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false);
    if ok {
        let _ = crate::agent::mcp::trust_project();
        println!("{}", style("✓ trusted — its tools are now available.").color256(splash::ACCENT));
    } else {
        let _ = crate::agent::mcp::dismiss_project();
        println!("{}", style("skipped — run `aizen mcp trust` anytime to enable.").color256(crate::ui::theme::FAINT));
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
/// up front so it shows exactly once (even if the user Ctrl-C's mid-setup); `ng config` reruns setup.
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
        eprintln!("{}", style("You can finish later with `aizen config`.").color256(crate::ui::theme::FAINT));
        return;
    }

    // Optional: connect a messaging app so the agent can reach the user (off by default — opt-in).
    let connect = Confirm::with_theme(&theme)
        .with_prompt("Connect a messaging app now? (Telegram / Discord / Slack / Webhook — optional)")
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
        style("Type to chat · / for commands · /apps for integrations.").color256(crate::ui::theme::FAINT)
    );
    // Discovery nudge for the specialist library (never auto-installs — just points the way).
    println!(
        "{}",
        style("Tip: add specialist sub-agents with `aizen agents install msitarzewski/agency-agents`.")
            .color256(crate::ui::theme::FAINT)
    );
}

/// One-line status string (model · tokens · turns · ctx% · yolo) for the sticky TUI footer. Plain
/// text — `paint_box` dims it as a whole.
fn status_text(history: &[Message], model: &str) -> String {
    let toks = session_tokens(history);
    let turns = history.iter().filter(|m| m.role == "user").count();
    let (window, _) = resolve_ctx_window(model);
    let pct = (toks as f64 / window as f64 * 100.0).min(100.0);
    let toklabel = if toks >= 1000 { format!("~{:.1}K", toks as f64 / 1000.0) } else { format!("~{toks}") };
    let winlabel = if window >= 1000 { format!("{}K", window / 1000) } else { window.to_string() };
    let mode = if yolo_enabled() {
        "  ·  ⚡ yolo"
    } else if smart_approve_enabled() {
        "  ·  ◆ smart"
    } else {
        ""
    };
    let todos = crate::agent::todo::status_summary().map(|s| format!("  ·  {s}")).unwrap_or_default();
    let cache = cache_hit_label().map(|s| format!("  ·  {s}")).unwrap_or_default();
    format!("{model}  ·  {toklabel}/{winlabel} tok  ·  {turns} turns  ·  {pct:.0}% ctx{cache}{todos}{mode}")
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

/// The sticky-TUI REPL: a background keyboard thread feeds a submission queue while the agent runs,
/// the input box stays pinned at the bottom, and Esc/Ctrl-C cancels an in-flight turn.
async fn run_menu_sticky() -> Result<()> {
    let http = http_client()?;
    let mut model_label = cli_config::load().model.unwrap_or_else(|| "(no model)".to_string());
    let mut history: Vec<Message> = Vec::new();
    rebuild_system(&mut history, &model_label);

    let intro = format!(
        "{}\n{}",
        splash::render(),
        style("Type to talk — messages queue while it works · Esc cancels a running turn · /help · /quit")
            .dim()
    );
    tui::activate(&intro, &status_text(&history, &model_label));
    let mut input = tui::spawn_input();

    loop {
        let sub = match input.submissions.recv().await {
            Some(s) => s,
            None => break,
        };
        match sub {
            tui::Submission::Quit => break,
            tui::Submission::Slash(cmd) => {
                let name = cmd.split(char::is_whitespace).next().unwrap_or("").trim();
                if cmd.trim().is_empty() || slash_is_interactive(name) {
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
                let echo = if echo_src.is_empty() { "(image)".to_string() } else { echo_src };
                tui::emit_line(&format!("{} {}", style("❯").color256(splash::ACCENT).bold(), echo));
                let (base_url, api_key, model) = match resolve_endpoint(None, None, None) {
                    Ok(t) => t,
                    Err(_) => {
                        tui::emit_line(&style("Not set up yet — /config (or /model to pick a model).").dim().to_string());
                        continue;
                    }
                };
                // A quiet rotating tip under the message (Claude-Code style) — a discoverability
                // nudge that advances per turn. Empty when tips are off (`AIZEN_NO_TIPS`) or off-TTY.
                // Placed after the endpoint check so an unconfigured REPL doesn't burn a tip.
                let tip = tui::next_tip();
                if !tip.is_empty() {
                    tui::emit_line(&style(format!("  {}{}", icons::g(icons::tip()), tip)).dim().to_string());
                }
                model_label = model.clone();
                if images.is_empty() {
                    history.push(Message::user(line));
                } else {
                    tui::emit_line(
                        &style(format!("📎 {} image(s) attached", images.len())).color256(splash::ACCENT).to_string(),
                    );
                    history.push(Message::user_with_images(line, images));
                }
                let persona_before = cli_config::load().persona;
                let registry = match agent::builtin::default_registry_with_task(
                    http.clone(),
                    base_url.clone(),
                    api_key.clone(),
                    model.clone(),
                    false,
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
                    auto_approve: yolo_enabled(),
                    smart_approve: smart_approve_enabled(),
                    context_window: resolve_ctx_window(&model).0,
                    enable_self_review: cli_config::load().self_review.unwrap_or(false),
                    ..AgentConfig::default()
                };
                // Bridge LSP config → the manager (per-request timeout; auto-enable if configured).
                // Control is normally via `/lsp on|off`; `enable_lsp` is the config/flag path.
                crate::agent::lsp::LSP.set_request_timeout(cfg.lsp_request_timeout_secs);
                crate::agent::lsp::LSP
                    .set_edit_feedback(cli_config::load().lsp_edit_diagnostics.unwrap_or(true));
                if cfg.enable_lsp {
                    let _ = crate::agent::lsp::LSP.enable();
                }

                tui::clear_cancel(); // fresh turn → forget any Esc from a previous one
                while input.cancel.try_recv().is_ok() {} // drain any stale cancel
                // Arm LAST: the keyboard thread only queues a cancel once WORKING is true, so flipping
                // it after the clear+drain guarantees no Esc meant for THIS turn gets swallowed in the
                // arming window.
                tui::set_working(true);

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
                            client::stream_chat_with_tools_eager(http_ref, base, key, model_ref, &msgs, &defs, Some(&starter)).await
                        } else {
                            client::stream_chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
                        }
                    };
                    // Non-streaming summarizer for mid-loop auto-compaction (keeps the streamed display clean).
                    let sum_ep = summarizer_endpoint(base, key, model_ref);
                    let summarize = move |msgs: Vec<Message>| {
                        let ep = sum_ep.clone();
                        async move {
                            client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
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
                                    client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                                        .await
                                        .map(|t| t.content.unwrap_or_default())
                                }
                            }
                        });
                    let fut = agent::run_agent_loop_full(chat, summarize, oracle, &cfg, &registry, &mut history);
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

                match result {
                    None => {
                        tui::emit_line(&theme::muted("⏹ stopped.").to_string());
                        history.push(Message::assistant("(interrupted by user)".to_string()));
                        // Esc means "stop" — also clear any queued submissions (type-ahead backlog or
                        // a stray multi-line paste) so one Esc halts everything instead of the next
                        // queued turn auto-firing.
                        let mut flushed = 0usize;
                        while input.submissions.try_recv().is_ok() {
                            flushed += 1;
                        }
                        if flushed > 0 {
                            tui::emit_line(
                                &theme::muted(format!("  cleared {flushed} queued message(s).")).to_string(),
                            );
                        }
                    }
                    // `clarify` paused the turn awaiting the user's answer — show the question and
                    // loop back to the input box (the next message continues this conversation).
                    // Skip the post-turn learning/compaction passes: the turn isn't finished yet.
                    Some(Ok(AgentOutcome { stop: StopReason::AwaitingInput(q), .. })) => {
                        show_clarify(&q);
                    }
                    Some(Ok(_)) => {
                        let persona_after = cli_config::load().persona;
                        if persona_after != persona_before {
                            update_system_prompt(&mut history, &model);
                            if let Some(name) = persona_after {
                                tui::emit_line(
                                    &style(format!("🎭 now playing: {name} (from your next message)"))
                                        .color256(splash::ACCENT)
                                        .to_string(),
                                );
                            }
                        }
                        maybe_learn_skill(&history, &http, &base_url, &api_key, &model).await;
                        maybe_evolve_persona(&history, &http, &base_url, &api_key, &model).await;
                        maybe_learn_memory(&history);
                        maybe_auto_compact(&mut history, &http, &base_url, &api_key, &model).await;
                        autosave_last(&history);
                    }
                    Some(Err(e)) => {
                        tui::emit_line(&format!("{} {e}", theme::err("error:")));
                        if history.last().map(|m| m.role == "user").unwrap_or(false) {
                            history.pop();
                        }
                    }
                }
                tui::set_status(&status_text(&history, &model_label));
            }
        }
    }
    tui::deactivate();
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

    let http = http_client()?;
    let mut model_label = cli_config::load().model.unwrap_or_else(|| "(no model)".to_string());
    let mut history: Vec<Message> = Vec::new();
    let mut input_history: Vec<String> = Vec::new(); // recallable past prompts (↑/↓ in the box)
    rebuild_system(&mut history, &model_label);

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
                println!("{}", style("Not set up yet — run /config (or /model to pick a model).").dim());
                continue;
            }
        };
        model_label = model.clone();
        if images.is_empty() {
            history.push(Message::user(line));
        } else {
            println!(
                "{}",
                style(format!("📎 {} image{} attached", images.len(), if images.len() == 1 { "" } else { "s" }))
                    .color256(splash::ACCENT)
            );
            history.push(Message::user_with_images(line, images));
        }
        // Snapshot the active persona so we can detect an in-turn switch (the `persona_create` tool)
        // and resync the system prompt at the turn boundary — prefix-cache safe, takes effect next msg.
        let persona_before = cli_config::load().persona;
        let registry = match agent::builtin::default_registry_with_task(
            http.clone(),
            base_url.clone(),
            api_key.clone(),
            model.clone(),
            false,
            resolve_ctx_window(&model).0,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {e}", style("error:").red());
                history.pop();
                continue;
            }
        };
        // auto_approve follows the `/yolo` toggle (or `AIZEN_YES`): on → destructive ops run without
        // a prompt; off (default) → each file edit / shell op asks first. `smart` (the `/smart`
        // toggle) auto-clears read-only shell commands when not in yolo.
        let cfg = AgentConfig {
            auto_approve: yolo_enabled(),
            smart_approve: smart_approve_enabled(),
            context_window: resolve_ctx_window(&model).0,
            enable_self_review: cli_config::load().self_review.unwrap_or(false),
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
                client::stream_chat_with_tools_eager(http_ref, base, key, model_ref, &msgs, &defs, Some(&starter)).await
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
                        client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                            .await
                            .map(|t| t.content.unwrap_or_default())
                    }
                }
            });
        match agent::run_agent_loop_full(chat, summarize, oracle, &cfg, &registry, &mut history).await {
            // `clarify` paused the turn — show the question, loop back for the answer (the next
            // typed message continues this conversation). No post-turn learning: not done yet.
            Ok(AgentOutcome { stop: StopReason::AwaitingInput(q), .. }) => {
                show_clarify(&q);
            }
            Ok(_) => {
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
                // Learn a skill from the full task detail BEFORE any compaction may summarize it.
                maybe_learn_skill(&history, &http, &base_url, &api_key, &model).await;
                // Let the active persona grow from this turn (free episode + periodic reflection).
                maybe_evolve_persona(&history, &http, &base_url, &api_key, &model).await;
                // Passively learn durable user/project facts (free regex; core stays human-gated).
                maybe_learn_memory(&history);
                maybe_auto_compact(&mut history, &http, &base_url, &api_key, &model).await;
                // Auto-checkpoint so /sessions can always restore where you left off (no manual save).
                autosave_last(&history);
            }
            Err(e) => {
                eprintln!("{} {e}", style("error:").red());
                if history.last().map(|m| m.role == "user").unwrap_or(false) {
                    history.pop(); // drop the failed user turn so history stays consistent
                }
            }
        }
    }
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
    // tui::emit_line routes through the sticky footer when active, else prints a plain line.
    tui::emit_line(&style(format!("↯ context {pct:.0}% ≥ {threshold}% — auto-compacting…")).dim().to_string());
    match compact_history(history, http, base, key, model).await {
        Ok((b, a)) => tui::emit_line(
            &style(format!("↯ auto-compacted: ~{} → ~{} tok", fmt_k(b), fmt_k(a))).color256(splash::ACCENT).to_string(),
        ),
        Err(e) => tui::emit_line(&format!("{} {e}", style("auto-compact skipped:").dim())),
    }
}

/// After a completed turn: if the just-finished task was a real multi-step PROCEDURE, distill it
/// into a reusable skill (the "self-evolving" path — like memory's free learning, but for how-to).
/// Conservative + best-effort: one cheap extraction call, only on substantial turns, skips when the
/// model says it isn't worth saving or a same-named skill already exists. Visible (prints a notice),
/// toggle in `/config`. Reads only the LAST task (from the last user message to the end).
async fn maybe_learn_skill(
    history: &[Message],
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    if !auto_skill_learn_enabled() {
        return;
    }
    let start = match history.iter().rposition(|m| m.role == "user") {
        Some(i) => i,
        None => return,
    };
    let turn = &history[start..];
    // Worth distilling iff EITHER the agent did substantial work (≥4 tool calls — a higher bar than
    // the old ≥2 so trivial 2-call turns stop minting noise skills) OR it RECOVERED from a dead end
    // (a tool errored, then a later call succeeded). The recovery path is exactly the hard-won
    // "do it THIS way next time" procedure worth saving even when it's short.
    let tool_calls: usize =
        turn.iter().filter(|m| m.role == "assistant").map(|m| m.tool_calls.len()).sum();
    if tool_calls < 4 && !turn_recovered_from_dead_end(turn) {
        return;
    }

    let existing: Vec<String> = skill::list().into_iter().map(|s| s.name).collect();
    let transcript = render_transcript(turn);
    let sys = Message::system(
        "You distill a COMPLETED task into a reusable skill, but ONLY if it is a generalizable, \
         repeatable procedure worth doing the same way next time. Be conservative — most one-off \
         tasks are NOT skills, and do not duplicate an existing skill. Reply with ONLY a JSON \
         object: {\"worth_saving\": true|false, \"name\": \"kebab-case-name\", \"when\": \"short \
         trigger\", \"steps\": \"1. ...\\n2. ...\"}. If not worth saving, reply {\"worth_saving\": false}.",
    );
    let usr = Message::user(format!(
        "Existing skills (do not duplicate): {}\n\nCompleted task transcript:\n{}",
        if existing.is_empty() { "(none)".to_string() } else { existing.join(", ") },
        transcript
    ));
    // Chore-class extraction call → billed to the summarizer role (env/config routing), never
    // silently to the main model.
    let ep = summarizer_endpoint(base, key, model);
    let resp = match client::chat_with_tools(http, &ep.base_url, &ep.api_key, &ep.model, &[sys, usr], &[]).await {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let content = resp.content.unwrap_or_default();
    let json = match extract_json_object(&content) {
        Some(j) => j,
        None => return,
    };
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return,
    };
    if !v.get("worth_saving").and_then(|b| b.as_bool()).unwrap_or(false) {
        return;
    }
    let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
    let steps = v.get("steps").and_then(|s| s.as_str()).unwrap_or("").trim();
    let when = v.get("when").and_then(|s| s.as_str()).unwrap_or("").trim();
    if name.is_empty() || steps.is_empty() {
        return;
    }
    // Don't overwrite/duplicate an existing skill (case-insensitive on the slug).
    let slug = skill::sanitize_name(name);
    if existing.iter().any(|e| skill::sanitize_name(e) == slug) {
        return;
    }
    // Auto-learned procedures are almost always about THIS project → they land in the current
    // workspace's zone, so another repo's `<skills>` index never pays for them.
    match skill::save_scoped(name, "", when, steps, true) {
        Ok(_) => tui::emit_line(
            &style(format!("{}learned skill '{name}' — /skills to view/edit/remove", icons::g(icons::learned())))
                .color256(splash::ACCENT)
                .to_string(),
        ),
        Err(_) => {} // best-effort
    }
}

/// Did this turn RECOVER from a dead end — a tool result errored, then a LATER tool result in the
/// same turn succeeded? That recovery is a hard-won procedure worth distilling even on a short turn.
/// Tool errors are fed back as result strings starting with `error:` (the loop's convention).
fn turn_recovered_from_dead_end(turn: &[Message]) -> bool {
    let mut saw_error = false;
    for m in turn.iter().filter(|m| m.role == "tool") {
        let is_err = m.content.as_deref().unwrap_or("").trim_start().to_ascii_lowercase().starts_with("error:");
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
/// FREE — regex extraction, no model call — through the SAME pipeline as `ng memory learn`
/// (sanitize-to-fact → write-time threat-scan → confidence-route → consolidate → store, with
/// anti-bloat). Core promotion stays human-gated (`auto_confirm_core = Some(false)`): a would-be
/// core fact is downgraded to a normal store entry and NEVER silently mutates the always-on frozen
/// prefix (prefix-cache byte-stability is sacred). Best-effort + visible; never disrupts the REPL.
fn maybe_learn_memory(history: &[Message]) {
    use crate::memory::learning::{self, LearnOptions};
    if !memory_auto_learn_enabled() {
        return;
    }
    let user_text =
        match history.iter().rev().find(|m| m.role == "user").and_then(|m| m.content.clone()) {
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
    let n = report.added.len() + report.reinforced.len();
    if n > 0 {
        tui::emit_line(
            &style(format!(
                "{}remembered {n} fact{} — /memory to view",
                icons::g(icons::learned()),
                if n == 1 { "" } else { "s" }
            ))
            .color256(splash::ACCENT)
            .dim()
            .to_string(),
        );
    }
}

/// After a completed turn: if a persona is active and evolution is on, let the character GROW.
/// Two-tier (Generative-Agents): (1) record a FREE episode of what it just lived through (zero
/// model cost), (2) when accumulated experience crosses a threshold, run ONE reflection call that
/// distills recent episodes into durable insights. Best-effort + visible — never disrupts the REPL.
async fn maybe_evolve_persona(
    history: &[Message],
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    if !persona_evolve_enabled() {
        return;
    }
    let persona = match persona::active() {
        Some(p) => p,
        None => return, // no character active → nothing to evolve
    };
    let slug = skill::sanitize_name(&persona.name);

    // Scope to the LAST turn (from the last user message to the end), like maybe_learn_skill.
    let start = match history.iter().rposition(|m| m.role == "user") {
        Some(i) => i,
        None => return,
    };
    let turn = &history[start..];
    let user_text = turn.first().and_then(|m| m.content.clone()).unwrap_or_default();
    let user_text = user_text.trim();
    if user_text.is_empty() {
        return;
    }
    let tool_calls: usize =
        turn.iter().filter(|m| m.role == "assistant").map(|m| m.tool_calls.len()).sum();
    let assistant_gist: String = turn
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && m.content.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();

    // Compact, first-person-ish episode body (bounded). The reflection pass reads these later.
    let outcome = if tool_calls >= 2 {
        format!("I worked through it across {tool_calls} tool steps")
    } else if tool_calls == 1 {
        "I used a tool to handle it".to_string()
    } else {
        "I answered directly".to_string()
    };
    let gist = if assistant_gist.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", truncate_chars(assistant_gist.trim(), 120))
    };
    let body = format!("The user asked: \"{}\". {outcome}{gist}.", truncate_chars(user_text, 200));

    let corrected = persona::self_mem::looks_like_correction(user_text);
    let importance = persona::self_mem::episode_importance(user_text, tool_calls, corrected);
    let _ = persona::self_mem::record_episode(&slug, &body, importance);

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
    let (sys, usr) = persona::reflect::build_reflection_prompt(&persona.name, &persona.role, &episodes);
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

/// Build the current system prompt (frozen core + persona + skills) for `model`.
fn current_system_prompt(model: &str) -> String {
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    agent::build_top_level_system_prompt(&cwd, std::env::consts::OS, &date, model, Some(&frozen))
}

/// Reset the conversation to just the system prompt (fresh session / model change). Rebuilds the
/// frozen core from the current memory store so newly added `type=user` facts / STYLE are injected.
fn rebuild_system(history: &mut Vec<Message>, model: &str) {
    history.clear();
    history.push(Message::system(current_system_prompt(model)));
}

/// Replace the system message in place WITHOUT clearing the conversation — used when switching
/// persona mid-chat so the new character applies but the history is preserved.
fn update_system_prompt(history: &mut Vec<Message>, model: &str) {
    let system = current_system_prompt(model);
    match history.first_mut() {
        Some(first) if first.role == "system" => *first = Message::system(system),
        _ => history.insert(0, Message::system(system)),
    }
}

/// Approximate context window (tokens) for a model, by name pattern. A rough heuristic for the
/// `% context` HUD only — not a hard cap (the upstream enforces the real limit). Defaults to 128K.
fn ctx_window_for(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("1m") {
        1_000_000 // explicit 1M-context variants (e.g. opus-4-8-1m-thinking) — checked before the family heuristics
    } else if m.contains("gemini") {
        1_000_000
    } else if m.contains("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
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
    let filled = ((pct / 100.0) * CELLS as f64).round().clamp(0.0, CELLS as f64) as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled));
    let color: u8 = if pct >= 80.0 { theme::ERR } else if pct >= 50.0 { theme::WARN } else { theme::OK };
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
    let configured = cfg.model_context_window.filter(|_| cfg.model.as_deref() == Some(model));
    effective_ctx_window(model, configured)
}

/// Rough session size in tokens — shared by the HUD + auto-compact. Delegates to the agent
/// estimator (content + tool-call payloads + envelopes) plus the tool-schema overhead the loop
/// last published, so the HUD and the mid-loop guards agree on request size.
fn session_tokens(history: &[Message]) -> usize {
    history.iter().map(agent::estimate_message_tokens).sum::<usize>() + agent::schema_overhead_tokens()
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
            style("(chars/4 — the provider didn't report token usage, so no per-call $ to show)").dim()
        ));
    }
}

/// Render a `clarify` question prominently and yield to the input box. `display` is the tool's
/// stored text: the question on the first line, any numbered options on the following lines.
/// Routes through `tui::emit_line` under the sticky TUI, else plain stdout — so the user just types
/// their answer next (it becomes the agent's next user turn). The dim `↳` hint sits below.
fn show_clarify(display: &str) {
    let mut lines = display.lines();
    let q = lines.next().unwrap_or("");
    let head = format!("{} {}", style("❓").color256(splash::ACCENT).bold(), style(q).bold());
    let opts: Vec<String> = lines.map(|l| style(l).color256(splash::ACCENT).to_string()).collect();
    let hint = style("↳ type your answer below to continue").dim().to_string();
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
                Ok(id) => tui::emit_line(&style(format!("🧠 remembered ({id})")).color256(splash::ACCENT).to_string()),
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
            tui::emit_line(&style("! — type a shell command after the !").dim().to_string());
            return InputPre::Handled;
        }
        match crate::agent::cmd_guard::classify(cmd) {
            crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                tui::emit_line(&format!("{} blocked by the safety floor: {reason}", theme::warn("✗")));
            }
            _ => {
                let out = run_shell_escape(cmd);
                tui::emit_line(&format!("{} {cmd}\n{out}", style("$").color256(splash::ACCENT)));
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
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(format!("chcp 65001>nul & {command}"));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            let s = s.trim_end().to_string();
            let s = if s.chars().count() > SHELL_ESCAPE_CAP {
                let head: String = s.chars().take(SHELL_ESCAPE_CAP).collect();
                format!("{head}\n…[output truncated]")
            } else {
                s
            };
            if s.is_empty() {
                format!("(exit {}, no output)", o.status.code().unwrap_or(-1))
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

/// "Yolo" mode — auto-approve destructive tools (file edits / shell) without prompting. Off by
/// default (safe). Forced on by the `AIZEN_YES` env var, else read from the persisted `/yolo` toggle.
fn yolo_enabled() -> bool {
    cli_config::branded_flag("YES") || cli_config::load().auto_approve.unwrap_or(false)
}

/// Whether the `smart` approval tier is on (auto-run read-only shell, ask for the rest). `None` ⇒ OFF.
fn smart_approve_enabled() -> bool {
    cli_config::load().smart_approve.unwrap_or(false)
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
    let tg = if telegram::is_configured() { "  ·  📱 telegram" } else { "" };
    let yolo = if yolo_enabled() {
        format!("  ·  {}", style("⚡ yolo").color256(theme::WARN)) // reserved gold — runs hot
    } else {
        String::new()
    };
    let (window, auto) = resolve_ctx_window(model);
    let pct = (toks as f64 / window as f64 * 100.0).min(100.0);
    let toklabel = if toks >= 1000 { format!("~{:.1}K", toks as f64 / 1000.0) } else { format!("~{toks}") };
    let winlabel = if window >= 1000 { format!("{}K", window / 1000) } else { window.to_string() };
    let tag = if auto { "ctx" } else { "ctx·est" }; // est = name-heuristic, provider didn't report it
    let ctx = format!("{} {}", ctx_bar(pct), style(format!("{pct:.0}% {tag}")).dim());
    // Auto-compact trigger level, plus how many times this session has actually compacted so far
    // (P-ctx3, read from the queryable boundary marker). `⊟ 80%` → `⊟ 80% ×2` after two compactions.
    let ac = match compact_threshold_pct() {
        0 => String::new(),
        t => {
            let n = agent::compact::compaction_count(history);
            let count = if n > 0 { format!(" ×{n}") } else { String::new() };
            style(format!("  ·  ⊟ {t}%{count}")).dim().to_string()
        }
    };
    let cache = cache_hit_label().map(|s| style(format!("  ·  {s}")).dim().to_string()).unwrap_or_default();
    let rest = style(format!(
        "{}{model}  ·  {toklabel}/{winlabel} tok  ·  {turns} turns{tg}",
        icons::g(icons::spark())
    ))
    .dim();
    // emit_line routes into the sticky scroll region (above the box) when active, else plain stdout.
    tui::emit_line(&format!("\n{rest}  ·  {ctx}{ac}{cache}{yolo}"));
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
/// (tokens_before, tokens_after). Same core the agent loop uses, so the REPL and `ng serve` compact
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
    let summary = client::chat_with_tools(&http, &ep.base_url, &ep.api_key, &ep.model, &prompt, &[])
        .await?
        .content
        .unwrap_or_default();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty handoff summary");
    }
    Ok(summary.trim().to_string())
}

enum SlashOutcome {
    Continue,
    Quit,
    /// A custom command expanded to this prompt — feed it through the normal chat path.
    Submit(String),
}

/// The slash commands, shown in the `/` picker (name, one-line description).
const SLASH_CMDS: &[(&str, &str)] = &[
    ("help", "show this list"),
    ("handoff", "start a fresh thread carrying only what matters for a new goal"),
    ("model", "list + pick the model (with context windows)"),
    ("config", "set endpoint + key + model (wizard)"),
    ("memory", "show your profile / search memory"),
    ("persona", "pick the character the agent role-plays (or author one)"),
    ("skills", "saved procedures — list/view/new/delete (the agent loads them)"),
    ("commands", "your custom slash commands (markdown macros you fire)"),
    ("apps", "connect apps (GitHub · Notion · Slack · Filesystem · …) as tools via MCP — needs npx/uvx for local apps"),
    ("mcp", "MCP servers from ~/.aizen/mcp.json (+ a trusted repo's ./.aizen/mcp.json) — list connected tools"),
    ("telegram", "Telegram integration menu (setup · test · status · daemon)"),
    ("sessions", "saved conversations — restore · save · delete (auto-saves as you go)"),
    ("timeline", "time machine — rewind / re-apply code states (git snapshots)"),
    ("checkpoint", "save a restore point of the code now"),
    ("compact", "summarize older turns to free context now"),
    ("lsp", "type-aware code navigation (references · definition · symbols · diagnostics) — on/off/status/restart"),
    ("reach", "web-access health check: which backend serves each platform (youtube · twitter · github · hn · wikipedia · feeds · stackexchange · search)"),
    ("yolo", "toggle auto-approve: run file edits & shell WITHOUT asking each time"),
    ("smart", "toggle smart approval: auto-run read-only shell, ask for the rest"),
    ("clear", "start a fresh conversation"),
    ("tokens", "show session token usage"),
    ("cost", "session token usage + $ estimate (real usage when the provider reports it)"),
    ("quit", "exit"),
];

/// Bare `/` → an arrow-key picker over the slash commands; runs the chosen one (default args).
/// User-defined custom commands are appended after the built-ins so they're discoverable too.
async fn slash_menu(history: &mut Vec<Message>, model_label: &mut String) -> SlashOutcome {
    let mut items: Vec<String> =
        SLASH_CMDS.iter().map(|(n, d)| format!("{}/{n}  —  {d}", icons::g(icons::slash(n)))).collect();
    let custom = commands::list();
    for c in &custom {
        let hint = if c.argument_hint.is_empty() { String::new() } else { format!(" {}", c.argument_hint) };
        let desc = if c.description.is_empty() { "(custom command)".to_string() } else { c.description.clone() };
        items.push(format!("{}/{}{hint}  —  {desc}", icons::g(icons::slash("commands")), c.name));
    }
    let theme = ui_theme();
    match Select::with_theme(&theme).with_prompt("slash command").items(&items).default(0).interact_opt() {
        Ok(Some(i)) if i < SLASH_CMDS.len() => handle_slash(SLASH_CMDS[i].0, history, model_label).await,
        // A custom command was picked — dispatch by name (no args from the picker).
        Ok(Some(i)) => handle_slash(&custom[i - SLASH_CMDS.len()].name, history, model_label).await,
        _ => SlashOutcome::Continue, // Esc / error → back to the prompt
    }
}

const SLASH_HELP: &str = "\
Commands:
  /help              this list
  /model             list the provider's models (with context windows) + pick one
  /config            set endpoint + key + model (wizard)
  /memory [query]    show your profile, or search memory
  /persona           pick the character the agent role-plays (list · select · new · clear · delete)
  /skills            saved procedures the agent can load (list · view · new · delete)
  /commands          your custom slash commands — markdown macros in ~/.aizen/commands/ ($ARGUMENTS · @file · !`cmd`)
  /apps              connected apps & MCP catalog — Telegram/Discord/Slack/webhook + browser sign-in apps
  /mcp               MCP servers from ~/.aizen/mcp.json — list connected tools
  /telegram          Telegram integration menu (setup · test · status · start daemon · disable)
  /sessions          saved conversations — restore · save · delete (auto-saves as you go)
  /timeline          time machine — rewind / re-apply code states (git snapshots); also /undo · /redo
  /checkpoint [note] save a restore point of the working tree now
  /compact           summarize older turns to free context now
  /lsp [on|off|status|restart]  type-aware navigation + diagnostics via a language server (rust-analyzer · pyright · typescript-language-server); default OFF, servers start lazily
  /reach [doctor|status]  web-access channels: live-probe every backend (doctor) or show what served this session (status); web_fetch/web_search route through these
  /yolo              toggle auto-approve — run file edits & shell WITHOUT asking each time
  /smart             toggle smart approval — auto-run read-only shell, ask for the rest
  /clear             start a fresh conversation
  /tokens            show session token usage (context-fill HUD)
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
fn slash_is_interactive(name: &str) -> bool {
    matches!(
        name,
        "sessions"
            | "model"
            | "models"
            | "config"
            | "setup"
            | "persona"
            | "personas"
            | "character"
            | "skills"
            | "skill"
            | "apps"
            | "integrations"
            | "telegram"
            | "tg"
            | "serve"
            | "memory"
            | "mem"
            | "timeline"
            | "tm"
    )
}

async fn handle_slash(input: &str, history: &mut Vec<Message>, model_label: &mut String) -> SlashOutcome {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    match name {
        "help" | "?" | "" => tui::emit_line(&style(SLASH_HELP).dim().to_string()),
        "quit" | "exit" | "q" => return SlashOutcome::Quit,
        "clear" | "new" | "reset" => {
            rebuild_system(history, model_label);
            crate::agent::todo::clear(); // a fresh conversation starts with an empty task list
            client::cost_meter().reset(); // and a fresh cost tally
            tui::reset_session_allow(); // re-confirm destructive ops in the new conversation
            tui::emit_line(&style("(new conversation)").dim().to_string());
        }
        "tokens" => print_status_line(history, model_label),
        "cost" | "usage" => print_cost(history, model_label),
        // /save + /load folded into /sessions (the current chat also auto-saves as "last").
        "save" | "load" => {
            tui::emit_line(&style("→ use /sessions — restore / save / delete are all there now").dim().to_string());
        }
        "sessions" => {
            if let Err(e) = sessions_menu(history).await {
                eprintln!("{} {e}", style("sessions:").red());
            }
        }
        "compact" => {
            tui::emit_line(&style("compacting…").dim().to_string());
            match compact_now(history).await {
                Ok((b, a)) => tui::emit_line(
                    &style(format!("compacted: ~{} → ~{} tok", fmt_k(b), fmt_k(a))).color256(splash::ACCENT).to_string(),
                ),
                Err(e) => tui::emit_line(&format!("{} {e}", style("compact:").red())),
            }
        }
        "handoff" => {
            if arg.trim().is_empty() {
                tui::emit_line(&style("usage: /handoff <new goal> — start a fresh thread carrying only what matters for it").dim().to_string());
            } else {
                tui::emit_line(&style("handing off…").dim().to_string());
                match handoff_now(history, arg.trim()).await {
                    Ok(summary) => {
                        // Fresh thread: new system prompt, the goal-relevant extraction seeded as
                        // context, todos cleared, destructive-op session grants re-armed (like /clear).
                        rebuild_system(history, model_label);
                        history.push(Message::system(format!(
                            "[handoff context from the previous session]\n{summary}"
                        )));
                        crate::agent::todo::clear();
                        tui::reset_session_allow();
                        tui::emit_line(&style("handoff — fresh thread seeded with the relevant context").color256(splash::ACCENT).to_string());
                        return SlashOutcome::Submit(arg.trim().to_string());
                    }
                    Err(e) => tui::emit_line(&format!("{} {e}", style("handoff:").red())),
                }
            }
        }
        "lsp" => {
            use crate::agent::lsp::LSP;
            let sub = arg.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => tui::emit_line(&LSP.status().render()),
                "on" | "enable" => match LSP.enable() {
                    Ok(_) => tui::emit_line(
                        &style("LSP on — references · definition · symbols · diagnostics (rust/python/js-ts; servers start lazily on first use; rust-analyzer can use ~1–3GB RAM). /lsp off to stop.")
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
        "yolo" | "auto" | "yes" => {
            let mut cfg = cli_config::load();
            let now = !cfg.auto_approve.unwrap_or(false);
            cfg.auto_approve = Some(now);
            match cli_config::save(&cfg) {
                Ok(_) if now => tui::emit_line(
                    &style("⚡ yolo ON — file edits & shell now run WITHOUT asking. /yolo again to turn it off.")
                        .color256(splash::ACCENT).to_string(),
                ),
                Ok(_) => tui::emit_line(&style("yolo OFF — destructive ops will ask for approval again.").dim().to_string()),
                Err(e) => tui::emit_line(&format!("{} {e}", style("yolo:").red())),
            }
            if cli_config::branded_flag("YES") {
                tui::emit_line(&style("(note: AIZEN_YES is set in your environment — it forces yolo ON regardless of this toggle)").dim().to_string());
            }
        }
        "smart" => {
            let mut cfg = cli_config::load();
            let now = !cfg.smart_approve.unwrap_or(false);
            cfg.smart_approve = Some(now);
            match cli_config::save(&cfg) {
                Ok(_) if now => tui::emit_line(
                    &style("◆ smart ON — read-only shell (ls/cat/rg/git status/cargo check) runs without asking; writes still prompt. /smart again to turn it off.")
                        .color256(splash::ACCENT).to_string(),
                ),
                Ok(_) => tui::emit_line(&style("smart OFF — every destructive op will ask for approval again.").dim().to_string()),
                Err(e) => tui::emit_line(&format!("{} {e}", style("smart:").red())),
            }
            if cfg.auto_approve.unwrap_or(false) || cli_config::branded_flag("YES") {
                tui::emit_line(&style("(note: yolo is ON — it approves everything, so smart has no extra effect until yolo is off)").dim().to_string());
            }
        }
        "model" | "models" => {
            // Merged: `/model` lists the provider's models (with context windows) AND picks one.
            if let Err(e) = slash_model(model_label).await {
                eprintln!("{} {e}", style("model:").red());
            } else {
                rebuild_system(history, model_label);
            }
        }
        "config" | "setup" => {
            if let Err(e) = config_wizard().await {
                eprintln!("{} {e}", style("config:").red());
            }
            *model_label = cli_config::load().model.unwrap_or_else(|| model_label.clone());
            rebuild_system(history, model_label);
        }
        "memory" | "mem" => {
            let r = if arg.is_empty() { memory::cmd_profile(false) } else { memory::cmd_search(arg, 5, None, None, None) };
            if let Err(e) = r {
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
            if let Err(e) = run_serve().await {
                eprintln!("{} {e}", style("serve:").red());
            }
        }
        // ── time machine (git snapshots) ──
        "timeline" | "tm" => {
            if let Err(e) = timeline_menu(history, model_label).await {
                eprintln!("{} {e}", style("time:").red());
            }
        }
        // Capture the conversation alongside the tree so this checkpoint supports the 3-way restore
        // (Files / Task / Both) later — a `/checkpoint` is a deliberate save point where the chat is
        // worth keeping, unlike the loop's per-edit auto-snapshots.
        "checkpoint" | "snapshot" | "cp" => match timemachine::save_with_chat(arg, false, history) {
            Ok(s) => tui::emit_line(&format!("{} #{} saved (code + chat)", style("✓ checkpoint").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("checkpoint: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        "undo" => match timemachine::undo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏪ rewound to").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("undo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        "redo" => match timemachine::redo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏩ re-applied").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("redo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // A user-defined command (`~/.nextgen/commands/<name>.md`) → expand its template and run it
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
    config::nextgen_home().join("sessions")
}
fn sanitize_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if n.is_empty() { "session".to_string() } else { n }
}
fn save_session(history: &[Message], name: &str) -> Result<String> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    config::harden_dir(&dir);
    let path = dir.join(format!("{}.json", sanitize_name(name)));
    std::fs::write(&path, serde_json::to_string_pretty(history)?).with_context(|| format!("writing {}", path.display()))?;
    // The transcript can contain pasted secrets / .env contents → owner-only on Unix.
    config::harden_file(&path);
    Ok(path.display().to_string())
}
fn load_session(history: &mut Vec<Message>, name: &str) -> Result<usize> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    let s = std::fs::read_to_string(&path).with_context(|| format!("no saved session '{name}'"))?;
    let loaded: Vec<Message> = serde_json::from_str(&s).context("parsing session file")?;
    *history = loaded;
    Ok(history.len())
}
fn list_sessions() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out.sort();
    out
}
/// Message count of a saved session (0 if missing/unparsable) — for the picker's display.
fn session_len(name: &str) -> usize {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Message>>(&s).ok())
        .map(|v| v.len())
        .unwrap_or(0)
}
fn delete_session(name: &str) -> Result<()> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}
/// Best-effort auto-save of the live conversation to the `last` session (called after each turn) so
/// you can always come back to it via `/sessions` without ever running an explicit save.
fn autosave_last(history: &[Message]) {
    if history.iter().any(|m| m.role == "user") {
        let _ = save_session(history, "last");
    }
}

/// `/sessions` — the conversation manager (replaces the old `/save` + `/load`): pick a saved
/// conversation to RESTORE, save the current one under a name, or delete one. The live chat is also
/// auto-saved as `last` after every turn, so there's always something to come back to.
async fn sessions_menu(history: &mut Vec<Message>) -> Result<()> {
    loop {
        let theme = ui_theme();
        let names = list_sessions();
        let n_sessions = names.len();
        let mut items: Vec<String> = names
            .iter()
            .map(|name| {
                let n = session_len(name);
                format!(
                    "{} {name}  —  {n} msg{}",
                    icons::g(icons::slash("sessions")),
                    if n == 1 { "" } else { "s" }
                )
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
            format!("Sessions — {n_sessions} saved · pick one to restore (Esc to go back)")
        };
        let pick = match Select::with_theme(&theme).with_prompt(prompt).items(&items).default(0).interact_opt()? {
            Some(i) => i,
            None => return Ok(()),
        };

        if pick < n_sessions {
            let name = &names[pick];
            match load_session(history, name) {
                Ok(n) => {
                    println!("{}", style(format!("restored '{name}' ({n} messages)")).color256(splash::ACCENT));
                    return Ok(());
                }
                Err(e) => eprintln!("{} {e}", style("restore:").red()),
            }
        } else if items[pick].starts_with("+ Save") {
            let name: String =
                Input::with_theme(&theme).with_prompt("Save as").interact_text().unwrap_or_default();
            if !name.trim().is_empty() {
                match save_session(history, name.trim()) {
                    Ok(_) => println!("{}", style(format!("saved '{}'", name.trim())).color256(splash::ACCENT)),
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
                match delete_session(&names[i]) {
                    Ok(_) => println!("{}", style(format!("deleted '{}'", names[i])).color256(splash::ACCENT)),
                    Err(e) => eprintln!("{} {e}", style("delete:").red()),
                }
            }
        } else {
            return Ok(()); // Back
        }
    }
}

/// `/model` — fetch the provider's models, pick one (arrow-key), persist it. Also captures the
/// context window when the provider reports it (→ a real `% context` HUD; else a name heuristic).
async fn slash_model(model_label: &mut String) -> Result<()> {
    let (base, key) = resolve_base_key(None, None)?;
    let http = http_client()?;
    let infos = client::fetch_models_info(&http, &base, &key).await.context("fetching models")?;
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
    let prompt = format!("Model ({} available, ↑/↓ to pick, Esc to cancel)", infos.len());
    let pick = match Select::with_theme(&theme).with_prompt(prompt).items(&items).default(idx).interact_opt()? {
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
    let winlabel = if window >= 1000 { format!("{}K", window / 1000) } else { window.to_string() };
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

/// Resolve just base URL + API key (for `aizen models`, which has no model).
fn resolve_base_key(base_url: Option<String>, api_key: Option<String>) -> Result<(String, String)> {
    let cfg = cli_config::load();
    let base_url = base_url
        .or_else(|| cli_config::branded_env("BASE_URL"))
        .or(cfg.base_url)
        .context("no base URL — pass --base-url, set AIZEN_BASE_URL, or run `aizen config set --base-url <url>`")?;
    let api_key = api_key
        .or_else(|| cli_config::branded_env("API_KEY"))
        .or(cfg.api_key)
        .context("no API key — pass --api-key, set AIZEN_API_KEY, or run `aizen config set --api-key <key>`")?;
    Ok((base_url, api_key))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        // Bound the connect + idle phases so a dead/stalled gateway can't freeze the terminal
        // forever. We deliberately set NO total `.timeout()` — chat responses stream and may run
        // for minutes; `read_timeout` caps the gap BETWEEN bytes (a silently stalled SSE stream),
        // not the whole stream. tcp_keepalive surfaces a half-open connection.
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

async fn run_config(cmd: Option<ConfigCmd>) -> Result<()> {
    let cmd = match cmd {
        Some(c) => c,
        None => return config_wizard().await, // bare `ng config` → interactive setup
    };
    match cmd {
        ConfigCmd::Set { base_url, api_key, model, context_window, compact_threshold, auto_skill_learn, memory_auto_learn, persona_evolve, price_in, price_out, icons, timemachine_keep } => {
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
                && timemachine_keep.is_none()
            {
                anyhow::bail!("nothing to set — pass at least one of --base-url / --api-key / --model / --context-window / --compact-threshold / --auto-skill-learn / --memory-auto-learn / --persona-evolve / --price-in / --price-out / --icons / --timemachine-keep");
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
            if let Some(k) = timemachine_keep {
                cfg.timemachine_keep = Some(k); // 0 = unlimited
            }
            cli_config::save(&cfg)?;
            println!("{} {}", crate::ui::theme::ok("✓"), style("saved").color256(splash::ACCENT));
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
/// of the wizard, after `config set`, and on `ng config show`. Plain `println!` (not the sticky
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
    let section = |name: &str| println!("\n  {} {}", theme::accent("◆"), theme::accent(name).bold());
    let row = |key: &str, val: String| println!("    {}  {val}", theme::muted(format!("{key:<8}")));
    let on = |b: bool| {
        if b {
            theme::ok("● on").to_string()
        } else {
            theme::faint("○ off").to_string()
        }
    };
    let unset = || theme::faint("— not set").italic().to_string();
    let tok = |n: usize| if n >= 1000 { format!("{}K", n / 1000) } else { n.to_string() };
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
    row("url", cfg.base_url.clone().map(|v| theme::link(redact_url(&v)).to_string()).unwrap_or_else(unset));
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
            let note = if was_cfg { "from provider" } else { "estimated by name" };
            row("context", format!("{} tok  {}", tok(w), theme::faint(format!("· {note}"))));
        }
        None => row("model", format!("{}  {}", unset(), theme::faint("· run /model"))),
    }

    // ── Session ──
    section("Session");
    row(
        "compact",
        match cfg.compact_threshold_pct.unwrap_or(80) {
            0 => format!("{}  {}", theme::faint("○ off"), theme::faint("· no auto-compaction")),
            t => format!("at {} of context", theme::accent(format!("{t}%"))),
        },
    );
    row("skills", format!("auto-learn {}", on(cfg.auto_skill_learn.unwrap_or(true))));
    row("memory", format!("auto-learn {}", on(cfg.memory_auto_learn.unwrap_or(true))));
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
            0 => format!("{}  {}", theme::accent("unlimited"), theme::faint("· keep every checkpoint")),
            k => format!("keep {}  {}", theme::accent(k.to_string()), theme::faint("· auto-prune oldest")),
        },
    );

    // ── Web search ──
    section("Web search");
    row(
        "tavily key",
        match cfg.reach.as_ref().and_then(|r| r.resolved_tavily_key()) {
            Some(k) => format!("{}  {}", cli_config::mask(&k), theme::ok("✓")),
            None => format!("{}  {}", unset(), theme::warn("web_search needs a key · run config")),
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
    row("icons", theme::accent(cfg.icons.as_deref().unwrap_or("nerd")).to_string());
    println!();
}

/// Given the fetched models + the currently-saved model, the index Select should default to.
fn model_default_index(models: &[String], current: Option<&str>) -> usize {
    current
        .and_then(|m| models.iter().position(|x| x == m))
        .unwrap_or(0)
}

const CUSTOM_MODEL_ITEM: &str = "‹ type a custom id ›";

/// Interactive setup (`ng config` with no subcommand, or menu → Setup): asks for base URL + a
/// hidden API key, fetches the model list, and lets you pick one with arrow keys. Enter keeps the
/// shown default at each step.
async fn config_wizard() -> Result<()> {
    let mut cfg = cli_config::load();
    let theme = ui_theme();
    let width = tui::width().clamp(46, 72);
    println!();
    println!("{}", style("Aizen · setup").bold().color256(splash::ACCENT));
    println!("{}", style(cli_config::config_path().display()).color256(crate::ui::theme::FAINT));
    println!("{}", style("Enter keeps the shown default at each step · Ctrl-C cancels").color256(crate::ui::theme::FAINT));
    println!("{}", style("─".repeat(width)).color256(crate::ui::theme::ACCENT_DIM));
    // Group the steps under gold section headers so the flow reads as Connection → Model → Behavior.
    let step = |label: &str| {
        println!("\n{} {}", style("◆").color256(splash::ACCENT), style(label).color256(splash::ACCENT).bold());
    };

    step("Connection");
    // 1) base URL
    let mut base_in = Input::<String>::with_theme(&theme).with_prompt("Base URL (OpenAI-compatible)");
    if let Some(cur) = cfg.base_url.clone() {
        base_in = base_in.default(cur);
    }
    let base = base_in.interact_text().context("reading base URL")?;
    let base = base.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        anyhow::bail!("base URL is required");
    }
    cfg.base_url = Some(base.clone());

    // 2) API key — hidden as you type; blank keeps the current one
    let key_prompt = match cfg.api_key.as_deref() {
        Some(k) => format!("API key (current {} — Enter to keep)", cli_config::mask(k)),
        None => "API key".to_string(),
    };
    let entered = Password::with_theme(&theme)
        .with_prompt(key_prompt)
        .allow_empty_password(true)
        .interact()
        .context("reading API key")?;
    if !entered.trim().is_empty() {
        cfg.api_key = Some(entered.trim().to_string());
    }
    if cfg.api_key.is_none() {
        anyhow::bail!("API key is required");
    }

    step("Model & context");
    // 3) fetch + pick a model (arrow-key Select, with a custom-id escape hatch)
    let http = http_client()?;
    print!("{} {base} … ", style("Fetching models from").dim());
    std::io::Write::flush(&mut std::io::stdout()).ok();
    match client::fetch_models_info(&http, &base, cfg.api_key.as_deref().unwrap()).await {
        Ok(infos) if !infos.is_empty() => {
            println!("{}", style(format!("ok ({} found)", infos.len())).dim());
            let ids: Vec<String> = infos.iter().map(|m| m.id.clone()).collect();
            let mut items: Vec<String> = ids.clone();
            items.push(CUSTOM_MODEL_ITEM.to_string());
            let pick = Select::with_theme(&theme)
                .with_prompt("Pick a model")
                .items(&items)
                .default(model_default_index(&ids, cfg.model.as_deref()))
                .interact()
                .context("picking a model")?;
            if pick < infos.len() {
                cfg.model = Some(infos[pick].id.clone());
                cfg.model_context_window = infos[pick].context_length; // auto when reported, else heuristic
            } else {
                let m: String = Input::with_theme(&theme).with_prompt("Model id").interact_text()?;
                if !m.trim().is_empty() {
                    cfg.model = Some(m.trim().to_string());
                    cfg.model_context_window = None; // custom id → heuristic
                }
            }
        }
        other => {
            match other {
                Ok(_) => println!("{}", style("no models returned.").dim()),
                Err(e) => {
                    println!("{}", style(format!("failed: {e}")).red());
                    eprintln!("{}", style("(the key or URL may be wrong — you can still set a model manually)").dim());
                }
            }
            let mut mi = Input::<String>::with_theme(&theme).with_prompt("Enter a model id manually");
            if let Some(cur) = cfg.model.clone() {
                mi = mi.default(cur);
            }
            let m = mi.allow_empty(true).interact_text()?;
            if !m.trim().is_empty() {
                cfg.model = Some(m.trim().to_string());
                cfg.model_context_window = None; // manual id, no provider metadata → heuristic
            }
        }
    }
    if cfg.model.is_none() {
        anyhow::bail!("a model is required (run `aizen models` to list them)");
    }

    // 4) context window — drives the `% context` HUD + the auto-compact trigger. The model pick
    //    above pre-filled `model_context_window` from the provider when it reported one; show that
    //    (or `auto`) as the default. A number overrides it; `auto` clears back to detect/heuristic.
    let model = cfg.model.clone().unwrap();
    let (shown, was_cfg) = effective_ctx_window(&model, cfg.model_context_window);
    let ctx_default = cfg.model_context_window.map(|w| w.to_string()).unwrap_or_else(|| "auto".to_string());
    let note = if was_cfg { "auto-detected from the provider" } else { "estimated from the model name" };
    println!("{}", style(format!("Context window — currently {shown} tokens ({note}).")).dim());
    let ctx_in = Input::<String>::with_theme(&theme)
        .with_prompt("Context window (tokens, e.g. 200000 / 128k, or `auto`)")
        .default(ctx_default)
        .allow_empty(true)
        .interact_text()?;
    cfg.model_context_window = match ctx_in.trim().to_ascii_lowercase().replace('_', "").replace('k', "000").parse::<usize>() {
        Ok(n) if n >= 1000 => Some(n),
        _ => None, // "auto"/blank/garbage → detect-or-heuristic
    };

    // Web search key (Tavily) — web_search is KEYED-ONLY now, so without a key it can't search.
    // Hidden as you type; blank keeps the current one; a lone `-` clears it back to unset.
    step("Web search");
    let cur_tavily = cfg.reach.as_ref().and_then(|r| r.tavily_api_key.clone());
    let tavily_prompt = match cur_tavily.as_deref() {
        Some(k) => format!("Tavily key (current {} — Enter keeps, `-` clears)", cli_config::mask(k)),
        None => "Tavily API key for web_search (free at tavily.com; Enter to skip)".to_string(),
    };
    println!("{}", style("web_search needs a Tavily key — env AIZEN_TAVILY_API_KEY overrides this.").dim());
    let tavily_in = Password::with_theme(&theme)
        .with_prompt(tavily_prompt)
        .allow_empty_password(true)
        .interact()
        .context("reading Tavily key")?;
    let tavily_in = tavily_in.trim();
    if !tavily_in.is_empty() {
        let reach = cfg.reach.get_or_insert_with(Default::default);
        reach.tavily_api_key = if tavily_in == "-" { None } else { Some(tavily_in.to_string()) };
    }

    step("Behavior");
    // 5) auto-compact threshold — % of the window at which older turns get summarized (`off` = 0).
    let cur_ac = cfg.compact_threshold_pct.unwrap_or(80);
    let ac_default = if cur_ac == 0 { "off".to_string() } else { cur_ac.to_string() };
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
    let sk_default = if cur_sk { "yes".to_string() } else { "no".to_string() };
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
    let ml_default = if cur_ml { "yes".to_string() } else { "no".to_string() };
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
        .default(if cur_tm == 0 { "unlimited".to_string() } else { cur_tm.to_string() })
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

    cli_config::save(&cfg)?;
    println!("\n{} {}", crate::ui::theme::ok("✓"), style("Saved.").color256(splash::ACCENT).bold());
    print_config(&cfg);
    println!("{}", style("Ready — type a message, or run:  aizen chat -p \"hello\"").color256(crate::ui::theme::FAINT));
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
        let mark = if current.as_deref() == Some(m.id.as_str()) { " (default)" } else { "" };
        let ctx = match m.context_length {
            Some(n) if n >= 1000 => format!("  · ctx {}K", n / 1000),
            Some(n) => format!("  · ctx {n}"),
            None => String::new(),
        };
        println!("{}{ctx}{mark}", m.id);
    }
    if !any_ctx {
        println!("\n{}", style("(this provider doesn't report context windows — the HUD estimates by model name)").dim());
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
    client::stream_chat(&http, &base_url, &api_key, &model, messages)
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

    // Session start: promote any frozen-core rebuild staged by a previous session, then
    // inject the (now-immutable) core as the always-on <user_memory> block.
    let _ = memory::frozen_core::promote_pending();
    let frozen = memory::frozen_core::read_active();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let system =
        agent::build_top_level_system_prompt(&cwd, std::env::consts::OS, &date, &model, Some(&frozen));

    // Registry includes the `task` sub-agent tool (depth 0); a spawned sub-agent uses a
    // role-scoped registry WITHOUT `task` (no recursion).
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        args.yes,
        resolve_ctx_window(&model).0,
    )?;
    let max = args.max_iters.unwrap_or(25).max(1);
    let cfg = AgentConfig {
        max_iters: max,
        auto_extend_to: max.saturating_mul(2),
        auto_approve: args.yes,
        context_window: resolve_ctx_window(&model).0,
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
            client::stream_chat_with_tools_eager(http_ref, base, key, model_ref, &msgs, &defs, Some(&starter)).await
        } else {
            client::stream_chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
        }
    };

    let outcome = agent::run_agent(chat, &cfg, &registry, &system, args.task.trim()).await?;
    match outcome.stop {
        // The final answer was already streamed to stdout during the call.
        StopReason::Done => {}
        StopReason::Divergence => eprintln!(
            "\n[stopped after {} steps: the model repeated the same tool call without progress]",
            outcome.iters
        ),
        StopReason::MaxIters => eprintln!(
            "\n[stopped: hit the step limit ({} steps) — the task may be incomplete]",
            outcome.iters
        ),
        // One-shot `ng agent` is non-interactive: there is no next message to answer with, so
        // surface the question and exit rather than hang. Re-run in the REPL to answer it.
        StopReason::AwaitingInput(q) => eprintln!(
            "\n[the agent needs clarification — re-run interactively (`aizen`) to answer]\n❓ {q}"
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

    agent::workflow::run_workflow(&http, &base_url, &api_key, &model, args.yes, &spec, trace).await
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
        MemoryCmd::List { scope } => memory::cmd_list(scope.as_deref()),
        MemoryCmd::Show { id } => memory::cmd_show(&id),
        MemoryCmd::Search { query, k, dimension, category, scope } => {
            memory::cmd_search(&query, k, dimension, category, scope.as_deref())
        }
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
        MemoryCmd::Review { promote, clear } => memory::cmd_review(promote, clear),
        MemoryCmd::AsOf { date } => memory::cmd_as_of(date.trim()),
        MemoryCmd::Supersede { old, new } => memory::cmd_supersede(&old, &new),
        MemoryCmd::Archive => memory::cmd_archive_list(),
        MemoryCmd::Restore { id } => memory::cmd_restore(&id),
        MemoryCmd::Compact => memory::cmd_compact(),
        MemoryCmd::Neighbors { id, k } => memory::cmd_neighbors(&id, k),
        MemoryCmd::ModelDownload { name } => {
            memory::model_dl::download(name.as_deref()).await.map(|_| ())
        }
    }
}

fn run_persona(cmd: PersonaCmd) -> Result<()> {
    match cmd {
        PersonaCmd::List => {
            let active_slug = cli_config::load().persona.as_deref().map(skill::sanitize_name);
            let ps = persona::list();
            if ps.is_empty() {
                println!("(no personas yet — `aizen persona new <name>`, or /persona in the REPL)");
                return Ok(());
            }
            for p in &ps {
                let slug = skill::sanitize_name(&p.name);
                let mark = if active_slug.as_deref() == Some(slug.as_str()) { "●" } else { "○" };
                let sub = if p.role.is_empty() { p.voice.clone() } else { p.role.clone() };
                let (eps, ins) = persona::self_mem::counts(&slug);
                println!("{mark} {} — {sub}  ({ins} insights, {eps} episodes)", p.name);
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
        PersonaCmd::New { name, role, voice, body } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading persona body from stdin")?,
            };
            let path = persona::save(&name, role.as_deref().unwrap_or(""), voice.as_deref().unwrap_or(""), &body)?;
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
                None => persona::active_slug()
                    .ok_or_else(|| anyhow::anyhow!("no active persona — pass a name or `aizen persona use <name>`"))?,
            };
            let label = persona::load(&slug).map(|p| p.name).unwrap_or_else(|| slug.clone());
            persona_self_view(&slug, &label);
            Ok(())
        }
        PersonaCmd::Remember { text, importance } => {
            let slug = persona::active_slug()
                .ok_or_else(|| anyhow::anyhow!("no active persona — `aizen persona use <name>` first"))?;
            let imp = importance
                .unwrap_or_else(|| {
                    persona::self_mem::episode_importance(&text, 0, persona::self_mem::looks_like_correction(&text))
                })
                .min(10);
            match persona::self_mem::record_episode(&slug, &text, imp)? {
                Some(id) => println!("recorded episode '{id}' (importance {imp})"),
                None => println!("(skipped — identical to the last episode)"),
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
                println!("(no skills — add one with `aizen skill add <name>`, or /skills in the REPL)");
                return Ok(());
            }
            for s in &skills {
                let d = if s.description.is_empty() { &s.when } else { &s.description };
                let tag = match s.origin {
                    skill::SkillOrigin::Global => "",
                    skill::SkillOrigin::Project => " [project]",
                    skill::SkillOrigin::Repo => " [repo]",
                };
                // Voyager provenance (v{N} · {M}× · updated …) — empty for a pristine, never-used v1.
                let prov = skill::version_tag(s);
                let prov = if prov.is_empty() { String::new() } else { format!("  ({prov})") };
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
        SkillCmd::Add { name, description, when, body } => {
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
        SkillCmd::Refine { name, description, when, body } => {
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
                anyhow::bail!("a search query is required, e.g. `aizen skill search deploy fastapi`");
            }
            let hits = skill_registry::search(&q, limit.unwrap_or(20).clamp(1, 50)).await?;
            if hits.is_empty() {
                println!("no skills on {} match '{q}'", skill_registry::registry_base());
                return Ok(());
            }
            println!(
                "{}",
                style(format!("{} result(s) from {} — install with `aizen skill install <owner/name>`", hits.len(), skill_registry::registry_base())).dim()
            );
            for sk in &hits {
                println!("{}", sk.summary_line());
            }
            Ok(())
        }
        SkillCmd::Install { slug } => {
            let sk = skill_registry::install(&slug).await?;
            println!("{} '{}' → {}", style("installed").color256(splash::ACCENT), sk.name, skill::skills_dir().join(format!("{}.md", skill::sanitize_name(&sk.name))).display());
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
    let resp = crate::agent::reach::http::get(&http, url, &[]).await.with_context(|| format!("GET {url}"))?;
    if !resp.is_success() {
        anyhow::bail!("upstream returned HTTP {}", resp.status);
    }
    let text = resp.text();
    // Fallback name from the URL's filename (strip a trailing .md).
    let stem = url.trim_end_matches('/').rsplit('/').next().unwrap_or("skill");
    let stem = stem.split(['?', '#']).next().unwrap_or(stem).trim_end_matches(".md");
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
    Ok(buf.strip_prefix('\u{FEFF}').unwrap_or(&buf).trim().to_string())
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
    if parts.len() == 2 && parts.iter().all(|p| !p.is_empty() && !p.contains(char::is_whitespace)) {
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
        Some(AgentsCmd::List { division, source, enabled, json }) => {
            agents_list(division.as_deref(), source.as_deref(), enabled, json)
        }
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
        Some(AgentsCmd::Install { source, yes, enable_all, as_name }) => {
            agents_install(&source, yes, enable_all, as_name.as_deref()).await
        }
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
    println!("  {}", style("aizen agents install msitarzewski/agency-agents").color256(splash::ACCENT));
    println!("…or drop `.md` personas into ~/.aizen/agents (or ~/.claude/agents).");
}

fn agents_list(division: Option<&str>, source: Option<&str>, enabled_only: bool, json: bool) -> Result<()> {
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

    let mut by_div: std::collections::BTreeMap<String, Vec<&agents::AgentDef>> = std::collections::BTreeMap::new();
    for a in &all {
        by_div
            .entry(a.division.clone().unwrap_or_else(|| "(no division)".to_string()))
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
        style("Installs write to ~/.aizen/agents; a higher-precedence dir wins on a slug collision.").dim()
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
    let base = s.trim_end_matches('/').rsplit(['/', ':', '\\']).next().unwrap_or(s);
    let base = base.trim_end_matches(".git");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() { "agents".to_string() } else { cleaned }
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
    let out = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--no-recurse-submodules", "--quiet", url])
        .arg(dest)
        .output()
        .context("running `git clone` (is git installed and on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Extract the host from a NON-http(s) git URL (`git@host:path`, `ssh://[user@]host[:port]/path`,
/// `git://host/path`) so the SSRF floor can guard it too. `None` if no host is discernible.
fn git_url_host(url: &str) -> Option<String> {
    let non_empty = |s: &str| {
        let s = s.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    };
    // scp-like: [user@]host:path (no scheme).
    if url.starts_with("git@") || (url.contains('@') && url.contains(':') && !url.contains("://")) {
        let after_at = url.rsplit('@').next().unwrap_or(url);
        return non_empty(after_at.split(':').next().unwrap_or(after_at));
    }
    let rest = url.strip_prefix("ssh://").or_else(|| url.strip_prefix("git://"))?;
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
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("md")) {
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
            std::fs::write(&dest, content).with_context(|| format!("writing {}", dest.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

async fn agents_install(source: &str, yes: bool, enable_all: bool, as_name: Option<&str>) -> Result<()> {
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
            let resp = crate::agent::reach::http::get(&http, &url, &[]).await.with_context(|| format!("GET {url}"))?;
            if !resp.is_success() {
                anyhow::bail!("upstream returned HTTP {}", resp.status);
            }
            let text = resp.text();
            if !agents::looks_like_agent(&text) {
                anyhow::bail!("that URL isn't an agent (needs frontmatter `name:` + a non-empty body)");
            }
            let stem = url.trim_end_matches('/').rsplit('/').next().unwrap_or("agent");
            let stem = stem.split(['?', '#']).next().unwrap_or(stem).trim_end_matches(".md");
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
            std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
            let (copied, skipped) = copy_agent_tree(&dir, &dest)?;
            crate::core::config::harden_dir(&agents::agents_dir());
            finish_install(copied, skipped, &dest, enable_all)
        }
        InstallSource::GitHubShorthand(slug) => {
            install_from_git(&format!("https://github.com/{slug}.git"), &slug, yes, enable_all).await
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
    if !yes && !confirm_write(&format!("Clone {url} and install its agents into ~/.aizen/agents?"))? {
        println!("cancelled.");
        return Ok(());
    }
    let repo = sanitize_repo_name(label);
    let dest = agents::agents_dir().join(&repo);
    let staging = std::env::temp_dir().join(format!("aizen-agents-clone-{}-{}", std::process::id(), unique_n()));
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

fn finish_install(copied: usize, skipped: usize, dest: &std::path::Path, enable_all: bool) -> Result<()> {
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
    println!("{}", style("review: `aizen agents list` · `aizen agents show <slug>`").dim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        vec!["opus-4-8".to_string(), "sonnet-4-6".to_string(), "minimax-m3".to_string()]
    }

    #[test]
    fn chunk_text_splits_on_utf16_units_not_scalars() {
        // 2100 emoji = 2100 scalars but 4200 UTF-16 units. Under a 3500-unit cap it MUST split —
        // Telegram/Discord count length in UTF-16; naive char-splitting would wrongly keep it whole
        // and the platform would 400 → the reply is silently dropped.
        let s = "🚀".repeat(2100);
        let chunks = chunk_text(&s, 3500);
        assert!(chunks.len() >= 2, "over-the-UTF16-cap reply must split, got {}", chunks.len());
        for c in &chunks {
            assert!(c.encode_utf16().count() <= 3500, "each chunk within the UTF-16 budget");
        }
        assert_eq!(chunks.concat(), s, "reassembles losslessly");
        assert_eq!(chunk_text("hello", 3500), vec!["hello".to_string()], "ASCII under cap stays whole");
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
    fn ctx_bar_uses_semantic_palette() {
        // P-ctx4: colour comes from the semantic palette (OK/WARN/ERR) at the 50%/80% thresholds,
        // not bespoke 256-indices. Force colour on so the ANSI code is actually emitted.
        console::set_colors_enabled(true);
        assert!(ctx_bar(30.0).contains(&theme::OK.to_string()), "green below 50%");
        assert!(ctx_bar(60.0).contains(&theme::WARN.to_string()), "gold from 50%");
        assert!(ctx_bar(90.0).contains(&theme::ERR.to_string()), "salmon from 80%");
    }

    #[test]
    fn classify_source_covers_the_matrix() {
        use super::InstallSource::*;
        // GitHub shorthand
        assert_eq!(classify_source("msitarzewski/agency-agents").unwrap(), GitHubShorthand("msitarzewski/agency-agents".into()));
        // git URLs (https repo, .git, scp-like, ssh)
        assert_eq!(classify_source("https://github.com/owner/repo").unwrap(), GitUrl("https://github.com/owner/repo".into()));
        assert_eq!(classify_source("https://github.com/owner/repo.git").unwrap(), GitUrl("https://github.com/owner/repo.git".into()));
        assert_eq!(classify_source("git@github.com:owner/repo.git").unwrap(), GitUrl("git@github.com:owner/repo.git".into()));
        assert_eq!(classify_source("ssh://git@host/owner/repo").unwrap(), GitUrl("ssh://git@host/owner/repo".into()));
        // single .md file (plain + query-stripped)
        assert_eq!(classify_source("https://example.com/a/code-reviewer.md").unwrap(), FileUrl("https://example.com/a/code-reviewer.md".into()));
        assert_eq!(classify_source("https://example.com/x.md?token=abc").unwrap(), FileUrl("https://example.com/x.md?token=abc".into()));
        // local dir forms
        assert!(matches!(classify_source("./local").unwrap(), LocalDir(_)));
        assert!(matches!(classify_source("/abs/path").unwrap(), LocalDir(_)));
        assert!(matches!(classify_source(".\\win").unwrap(), LocalDir(_)));
        assert!(matches!(classify_source("C:\\Users\\me\\agents").unwrap(), LocalDir(_)));
        // errors: not a path, not a url, not owner/repo
        assert!(classify_source("a/b/c").is_err(), "3-segment is not shorthand");
        assert!(classify_source("two words").is_err());
        assert!(classify_source("   ").is_err());
    }

    #[test]
    fn sanitize_repo_name_extracts_clean_dir() {
        assert_eq!(sanitize_repo_name("msitarzewski/agency-agents"), "agency-agents");
        assert_eq!(sanitize_repo_name("https://github.com/owner/repo.git"), "repo");
        assert_eq!(sanitize_repo_name("git@github.com:owner/repo.git"), "repo");
        assert_eq!(sanitize_repo_name("/some/local/My Agents"), "My-Agents");
    }

    #[test]
    fn git_url_host_extracts_host_for_ssrf_guard() {
        assert_eq!(git_url_host("git@github.com:owner/repo.git").as_deref(), Some("github.com"));
        assert_eq!(git_url_host("git@10.0.0.5:a/b.git").as_deref(), Some("10.0.0.5"));
        assert_eq!(git_url_host("ssh://git@host.example/owner/repo").as_deref(), Some("host.example"));
        assert_eq!(git_url_host("ssh://host:22/path").as_deref(), Some("host"));
        assert_eq!(git_url_host("git://internal/repo").as_deref(), Some("internal"));
        // http(s) are guarded on the path directly, not via this extractor.
        assert_eq!(git_url_host("https://github.com/o/r"), None);
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
        assert_eq!(h[1].role, "user", "tail begins at a user boundary (no orphaned turn)");
        // the most recent turn must survive
        assert!(h.iter().any(|m| m.content.as_deref() == Some("u3")));
    }

    #[test]
    fn cap_session_keeps_single_turn_even_if_over_cap() {
        // One huge turn can't be split at a 2nd user boundary → left intact (loop guard handles size).
        let mut h = vec![Message::system("sys"), Message::user("u1"), Message::assistant("a1")];
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
        assert!(turn_recovered_from_dead_end(&recovered), "error then later success = recovery");

        let no_error = vec![Message::user("x"), Message::tool_result("1", "fine"), Message::tool_result("2", "ok")];
        assert!(!turn_recovered_from_dead_end(&no_error), "no error → no recovery");

        let only_error = vec![Message::user("x"), Message::tool_result("1", "error: boom")];
        assert!(!turn_recovered_from_dead_end(&only_error), "error with no later success → no recovery");
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
        assert_eq!(h[cut].role, "user", "cut index {cut} is not a user boundary");
        assert!(cut > 1, "must summarize at least one older message");
        // KEEP_TURNS=3, three user turns → keep last 2 → cut at the 2nd user (index 5).
        assert_eq!(cut, 5);
    }

    #[test]
    fn compact_keeps_short_conversations_intact() {
        let k = COMPACT_KEEP_TURNS;
        assert_eq!(agent::compact::plan_compact_cut(&[Message::system("s")], k), None);
        assert_eq!(agent::compact::plan_compact_cut(&[Message::system("s"), Message::user("u")], k), None);
        // one full turn (1 user) → not worth compacting
        assert_eq!(
            agent::compact::plan_compact_cut(&[Message::system("s"), Message::user("u"), Message::assistant("a")], k),
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
}
