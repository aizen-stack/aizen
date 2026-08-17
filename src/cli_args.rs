//! The command-line surface: the whole `clap` type tree for `aizen`.
//!
//! Pure declaration — no behavior. Every subcommand enum lives here so that adding or reshaping a
//! command touches ONE file, and `main.rs` keeps only the dispatch that maps a parsed command to a
//! runner. The types are `pub(crate)` because `main.rs` matches on them; nothing else should.

use crate::features::cron;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
// No explicit `name` — clap uses the package name ("aizen") for `--version` and the actual argv[0]
// (aizen / ng) for the usage string, so each command name prints itself.
#[command(
    version,
    about = "Aizen agentic CLI — streaming chat + a self-learning memory brain"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
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
    /// Provider sign-in (experimental). Currently: ChatGPT Codex OAuth.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
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
        /// Report whether a `serve` daemon on this machine is alive, then exit 0 (healthy) or 1.
        /// Reads the heartbeat the daemon writes; shaped for a container/systemd exec probe. The
        /// daemon listens on no port (Telegram long-polls, Discord dials out), so there is no
        /// `/healthz` to curl — and adding one would forfeit the run-behind-NAT property.
        #[arg(long)]
        health: bool,
        /// Host only these extra bots (comma-separated names from `/addbot`), e.g.
        /// `--bots work,ops`. Telegram allows exactly ONE poller per token, so running the same bot
        /// on two machines is a 409 fight, not redundancy — this is how a fleet divides them. The
        /// primary bot always runs. Also settable as `AIZEN_SERVE_BOTS`. Alternatively pin a bot to
        /// a machine with the `host` field in `hostbot/bots.json`.
        #[arg(long, value_delimiter = ',', env = "AIZEN_SERVE_BOTS")]
        bots: Vec<String>,
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
    /// Other aizen windows working in this repository: status · diff · claims · commit.
    Team {
        #[command(subcommand)]
        cmd: TeamCmd,
    },
    /// Isolated Git worktrees, one per parallel task: new · list · remove.
    Work {
        #[command(subcommand)]
        cmd: WorkCmd,
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
    /// Show a byte breakdown of what every turn resends: the system prompt (static base +
    /// environment + memory lanes) and the JSON tool schemas. Runs offline — no request is made.
    #[command(name = "prompt-size")]
    PromptSize {
        /// Model id to size for (the prompt tier depends on it). Defaults to the configured model.
        #[arg(short, long)]
        model: Option<String>,
        /// Per-tool schema sizes, largest first.
        #[arg(long)]
        tools: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Render the moonlit braille art scene (one frame) to the terminal.
    Art,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AppsCmd {
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
pub(crate) enum AgentsCmd {
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
    /// Set (or clear) the provider/model assignment for a specialist.
    #[command(name = "set-provider")]
    SetProvider {
        /// Agent name or slug.
        name: String,
        /// Saved provider profile name.
        provider: Option<String>,
        /// Optional model override; omitted uses the provider default.
        model: Option<String>,
        /// Clear the assignment and inherit the sub-agent default.
        #[arg(long)]
        clear: bool,
    },
    /// Pin (or clear) the legacy `model:` field on a specialist card. Prefer set-provider for normal use.
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
pub(crate) enum McpCmd {
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
pub(crate) enum SkillCmd {
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
    /// Print the three folders skills are read from, with file counts. The `[project]`/`[repo]` tags
    /// in `skill list` say which folder a skill came from but not where that folder IS.
    Where,
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
    /// Retire a skill (archived under `.archive/`, not erased — see `restore`).
    Delete {
        /// Skill name.
        name: String,
    },
    /// Restore a retired skill by name.
    Restore {
        /// Skill name, as shown in `skill list`'s retired line.
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
pub(crate) enum PersonaCmd {
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
        /// Show every insight and episode, not just the head of each list.
        #[arg(short, long)]
        all: bool,
    },
    /// Record a free self-memory episode for the ACTIVE persona (no model call).
    Remember {
        /// What the character lived through.
        text: String,
        /// Importance 0–10 (else auto-scored).
        #[arg(short, long)]
        importance: Option<u8>,
    },
    /// Retire one self-memory (insight/episode) by id — archived, not erased.
    Forget {
        /// The self-memory id, as shown by `persona self`.
        id: String,
        /// Persona name (else the active one).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Restore a retired self-memory by id.
    #[command(name = "unforget")]
    Unforget {
        /// The self-memory id.
        id: String,
        /// Persona name (else the active one).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Retire a persona (card + its self-memory are archived, not erased).
    Delete {
        /// Persona name.
        name: String,
    },
    /// Restore a retired persona card.
    Restore {
        /// Persona name.
        name: String,
    },
    /// Print the assembled `<persona>` + `<self>` blocks the model actually sees.
    Block,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SoulCmd {
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
pub(crate) enum TelegramCmd {
    /// Interactive setup: paste the @BotFather token, then message the bot to capture your chat id.
    Setup,
    /// Send a test message to the configured chat (validates token + chat id).
    Test,
    /// Show the Telegram config (token redacted).
    Show,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ReachCmd {
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
pub(crate) enum TimeCmd {
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
    ///
    /// With `--all`, instead sweeps EVERY private store under `~/.aizen/timemachine/` and reports
    /// orphan stores whose source repository no longer exists (deleted or moved). Dry-run unless
    /// `--apply` is given, which moves orphans to `~/.aizen/timemachine/.trash/<timestamp>/`.
    Gc {
        /// Sweep every repo's store at the home level (orphan-store cleanup), not just this repo.
        #[arg(long)]
        all: bool,
        /// With `--all`: actually move orphan stores to `.trash/` (default is a dry-run report).
        #[arg(long)]
        apply: bool,
    },
    /// Delete ALL checkpoints (Git objects are reclaimed later by normal Git maintenance).
    Clear,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TeamCmd {
    /// List every aizen session in this repository: state, task, files touched, overlaps.
    Status,
    /// Show what ONE session changed, measured from its own pre-edit checkpoints.
    Diff {
        /// Session id, a unique suffix of one, `self`, or a row number from `status`.
        session: String,
        /// Print the unified patch, not just the per-file stat.
        #[arg(short, long)]
        patch: bool,
    },
    /// Show which session currently owns each changed path.
    Claims,
    /// Stage exactly one session's files and review them. Committing requires `--yes`.
    Commit {
        /// Session id, a unique suffix of one, `self`, or a row number from `status`.
        session: String,
        /// Commit message. Defaults to that session's task description.
        #[arg(short, long)]
        message: Option<String>,
        /// Stage and print the review, then unstage without committing.
        #[arg(long)]
        dry_run: bool,
        /// Proceed even when the session is still running or its files overlap another session's.
        #[arg(long)]
        force: bool,
        /// Actually commit. Without it, this behaves as `--dry-run`.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorkCmd {
    /// Create an isolated worktree + branch (`aizen/<name>`) and print where it landed.
    New {
        /// Worktree name: letters, digits, `-`, `_`, `.` (1-64 chars).
        name: String,
    },
    /// List aizen worktrees with their branch, dirty state, unmerged commits, and live sessions.
    List,
    /// Remove an aizen worktree. Refuses while it holds uncommitted or unmerged work.
    Remove {
        /// Worktree name.
        name: String,
        /// Remove anyway. The branch is kept either way, so commits stay reachable by name.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum DiscordCmd {
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
pub(crate) struct CrawlArgs {
    /// Seed URL(s) to crawl (absolute http(s)). Repeatable.
    #[arg(required = true)]
    pub(crate) urls: Vec<String>,
    /// Max crawl depth (hops from a seed).
    #[arg(short, long, default_value_t = 2)]
    pub(crate) depth: usize,
    /// Hard ceiling on the number of discovered URLs.
    #[arg(long, default_value_t = 200)]
    pub(crate) max_pages: usize,
    /// Scope: `strict` (same host) or `subs` (same root domain + subdomains).
    #[arg(long, default_value = "strict")]
    pub(crate) scope: String,
    /// Concurrent fetches.
    #[arg(short, long, default_value_t = 10)]
    pub(crate) concurrency: usize,
    /// Per-request timeout (seconds).
    #[arg(long, default_value_t = 15)]
    pub(crate) timeout: u64,
    /// Emit JSON ({url, depth, via}) instead of one URL per line.
    #[arg(long)]
    pub(crate) json: bool,
    /// Annotate each URL with its source (seed/html/js) in plain output.
    #[arg(long)]
    pub(crate) show_source: bool,
}

/// `aizen auth …` — experimental provider OAuth (ChatGPT Codex).
#[derive(Subcommand, Debug)]
pub(crate) enum AuthCmd {
    /// Browser PKCE login for ChatGPT Codex (experimental / ToS risk).
    Login {
        /// Provider id. Only `codex` is supported today.
        provider: String,
    },
    /// Show login state (never prints raw tokens).
    Status,
    /// Delete cached tokens for a provider.
    Logout { provider: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCmd {
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
        /// Model for compaction/handoff summaries (`roles.summarizer`) — the classic cheap-model
        /// slot. Empty clears.
        #[arg(long)]
        summarizer_model: Option<String>,
        /// Base URL for the summarizer endpoint (`roles.summarizer.base_url`). Empty clears.
        #[arg(long)]
        summarizer_base_url: Option<String>,
        /// API-key reference for the summarizer endpoint: `env:VAR` (preferred) or a literal key.
        /// Empty clears. (`roles.summarizer.api_key_ref`.)
        #[arg(long)]
        summarizer_api_key_ref: Option<String>,
        /// Model for the self-review reviewer (`roles.oracle`) — a stronger model is the point.
        /// NOTE: configuring this role also TURNS SELF-REVIEW ON unless `self_review` says otherwise.
        /// Empty clears.
        #[arg(long)]
        oracle_model: Option<String>,
        /// Base URL for the oracle endpoint (`roles.oracle.base_url`). Empty clears.
        #[arg(long)]
        oracle_base_url: Option<String>,
        /// API-key reference for the oracle endpoint: `env:VAR` (preferred) or a literal key.
        /// Empty clears. (`roles.oracle.api_key_ref`.)
        #[arg(long)]
        oracle_api_key_ref: Option<String>,
        /// Model for the reserved fast-apply edit role (`roles.apply`; config-only today). Empty clears.
        #[arg(long)]
        apply_model: Option<String>,
        /// Base URL for the apply endpoint (`roles.apply.base_url`). Empty clears.
        #[arg(long)]
        apply_base_url: Option<String>,
        /// API-key reference for the apply endpoint: `env:VAR` (preferred) or a literal key.
        /// Empty clears. (`roles.apply.api_key_ref`.)
        #[arg(long)]
        apply_api_key_ref: Option<String>,
        /// Register a model→endpoint mapping so a sub-agent pinned to that model carries its own
        /// gateway. Format: `model[,base_url=URL][,api_key_ref=env:VAR|KEY]` (repeatable). A bare
        /// model id with no fields, or `model,clear`, removes the entry.
        #[arg(long = "model-endpoint")]
        model_endpoint: Vec<String>,
    },
    /// Manage named main-endpoint profiles for quick manual failover.
    Provider {
        #[command(subcommand)]
        cmd: ProviderConfigCmd,
    },
    /// Show the saved config (API key masked).
    Show,
    /// Print the config file path.
    Path,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ProviderConfigCmd {
    /// Add a named endpoint profile. Existing names are rejected.
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        api_key: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        context_window: Option<usize>,
        /// Activate this profile immediately after saving it.
        #[arg(long = "use")]
        activate: bool,
    },
    /// Edit every field of an existing profile.
    Edit {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        api_key: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        context_window: Option<usize>,
    },
    /// Rename a profile and update role/specialist references.
    Rename { name: String, new_name: String },
    /// Activate a saved profile.
    Use { name: String },
    /// List saved profiles (keys masked).
    List,
    /// Remove a saved profile. Referenced profiles require --replace-with or --force.
    Remove {
        name: String,
        #[arg(long)]
        replace_with: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Parser, Debug)]
pub(crate) struct ModelsArgs {
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    pub(crate) base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    pub(crate) api_key: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum BenchCmd {
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
pub(crate) struct ChatArgs {
    /// One-shot prompt. If omitted, the prompt is read from stdin.
    #[arg(short, long)]
    pub(crate) prompt: Option<String>,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    pub(crate) base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    pub(crate) api_key: Option<String>,
    /// Model id (else AIZEN_MODEL / saved config).
    #[arg(short, long, env = "AIZEN_MODEL")]
    pub(crate) model: Option<String>,
}

#[derive(Parser, Debug)]
pub(crate) struct AgentArgs {
    /// The task for the agent to accomplish.
    pub(crate) task: String,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    pub(crate) base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    pub(crate) api_key: Option<String>,
    /// Model id (else AIZEN_MODEL / saved config).
    #[arg(short, long, env = "AIZEN_MODEL")]
    pub(crate) model: Option<String>,
    /// Pre-authorize destructive tools (file edits / shell) without an interactive prompt.
    #[arg(short, long)]
    pub(crate) yes: bool,
    /// Hard step cap before the one-shot auto-extend (default 25).
    #[arg(long)]
    pub(crate) max_iters: Option<usize>,
}

#[derive(Parser, Debug)]
pub(crate) struct WorkflowArgs {
    /// Path to a workflow spec (JSON): {name, tasks:[{id,role,prompt,model?}], synthesis?:{model?,prompt?}}.
    pub(crate) spec: String,
    /// OpenAI-compatible base URL (else AIZEN_BASE_URL / saved config).
    #[arg(long, env = "AIZEN_BASE_URL")]
    pub(crate) base_url: Option<String>,
    /// Bearer API key (else AIZEN_API_KEY / saved config).
    #[arg(long, env = "AIZEN_API_KEY")]
    pub(crate) api_key: Option<String>,
    /// Default model id for the sub-agents + synthesis (a task's `model` field overrides it for
    /// that task). Else AIZEN_MODEL / saved config.
    #[arg(short, long, env = "AIZEN_MODEL")]
    pub(crate) model: Option<String>,
    /// Pre-authorize destructive tools (file edits / shell) for the sub-agents without prompts.
    #[arg(short, long)]
    pub(crate) yes: bool,
    /// Write a JSON audit trace of the fan-out (per-task model + outcome + synthesis model) here.
    #[arg(long)]
    pub(crate) trace: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MemoryCmd {
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
        scope: Option<String>,
        /// Same as the positional form. Kept because the flag shipped first and scripts use it.
        #[arg(long = "scope", value_name = "SCOPE", conflicts_with = "scope")]
        scope_flag: Option<String>,
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
    /// Print the folders the store lives in, with file counts — for editing or clearing out many
    /// entries at once, which no per-id command can do.
    Where,
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

// ───────────────────────── project identity (where + zones) ─────────────────────────

#[derive(Subcommand, Debug)]
pub(crate) enum ZoneCmd {
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
