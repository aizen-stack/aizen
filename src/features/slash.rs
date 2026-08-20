//! Shared registry for user-invocable slash commands.
//!
//! The picker, live input palette, and help text all consume this catalog so a command cannot be
//! executable yet invisible in one of the UI surfaces. Compatibility aliases are listed explicitly:
//! they remain useful to people who type them, while the primary commands keep the first position.

use super::commands;

/// One command shown by a slash-command surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Name without the leading `/`.
    pub name: String,
    /// Short description used by menus and the live palette.
    pub description: String,
    /// Optional argument syntax shown by the dialoguer picker and live palette.
    pub argument_hint: String,
    /// Whether this entry came from a markdown custom command.
    pub custom: bool,
}

/// Canonical identity of a built-in slash command.
///
/// `handle_slash` matches on this EXHAUSTIVELY, so a new variant does not compile until a
/// handler exists for it. That is the point: name, aliases, help text, whether it takes
/// arguments and whether it owns stdin all live in ONE row of [`BUILTINS`], and the compiler
/// refuses to let dispatch drift away from that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashId {
    Help,
    Init,
    Where,
    Handoff,
    Goal,
    Model,
    Provider,
    Config,
    Memory,
    Persona,
    Skills,
    Commands,
    Apps,
    Mcp,
    Browser,
    Telegram,
    Serve,
    Sessions,
    Import,
    Resume,
    Workflows,
    Work,
    Agents,
    Recover,
    Timemachine,
    Checkpoint,
    Diff,
    Compact,
    Lsp,
    Reach,
    Approval,
    Sandbox,
    Effort,
    Ultimate,
    Clear,
    Tokens,
    Context,
    Cost,
    Tools,
    Update,
    Undo,
    Redo,
    Quit,
    Save,
Smart,
    Team,
    Yolo,
    AutoCopy,
}

/// When a command takes over stdin (a `dialoguer` menu, the effort slider, a daemon) and the
/// retained frame therefore has to suspend before it runs.
///
/// Argument-dependent by design: bare `/effort` drags a slider while `/effort high` just sets it,
/// and `/tools` prints while `/tools menu` opens a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    /// Pure print — runs with the box still up so its output lands in the scroll region.
    Never,
    /// Always opens something that owns the terminal.
    Always,
    /// Only with no argument.
    WhenBare,
    /// Only when the first argument word is one of these.
    WhenArg(&'static [&'static str]),
    /// With no argument, or when the first argument word is one of these.
    BareOr(&'static [&'static str]),
}

impl Stdin {
    fn claims(self, arg: &str) -> bool {
        let head = arg.split_whitespace().next().unwrap_or("");
        match self {
            Stdin::Never => false,
            Stdin::Always => true,
            Stdin::WhenBare => arg.is_empty(),
            Stdin::WhenArg(words) => words.iter().any(|w| head.eq_ignore_ascii_case(w)),
            Stdin::BareOr(words) => {
                arg.is_empty() || words.iter().any(|w| head.eq_ignore_ascii_case(w))
            }
        }
    }
}

/// One built-in command: its identity, every spelling that reaches it, and how it behaves.
pub struct Builtin {
    pub id: SlashId,
    /// Canonical name, without the leading `/`.
    pub name: &'static str,
    /// Compatibility spellings that ALSO get their own picker row (so someone who learned the old
    /// name still finds it), paired with the text that row shows.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Accepted, but not advertised — spellings we no longer want to teach.
    pub hidden_aliases: &'static [&'static str],
    /// The command itself is dispatchable but unlisted (superseded by a newer name).
    pub hidden: bool,
    pub description: &'static str,
    pub argument_hint: &'static str,
    /// The `/help` line. Longer and more specific than `description`, which has to fit inside a
    /// picker row. Empty falls back to `description`, so a new command is documented either way.
    pub help: &'static str,
    pub stdin: Stdin,
}

/// Every built-in command, in the order the picker shows them.
///
/// This is the ONLY place a command name is written down. `classify`, the `/` picker, the
/// live palette, the stdin-suspend rule and `handle_slash` all read it, so a command cannot
/// be executable-but-invisible (`/team`, `/providers` both were) or listed-but-dead.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        id: SlashId::Help,
        name: "help",
        aliases: &[],
        hidden_aliases: &["?"],
        hidden: false,
        description: "show commands and tips",
        argument_hint: "",
        help: "this list",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Init,
        name: "init",
        aliases: &[("index", "alias for /init")],
        hidden_aliases: &[],
        hidden: false,
        description: "index the codebase for semantic search + auto-retrieval",
        argument_hint: "[--force|--status]",
        help: "index the codebase into a semantic chunk index (SHA-256 incremental, secrets redacted); powers codebase_search + auto per-turn retrieval. --force rebuilds, --status shows state, Esc cancels",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Where,
        name: "where",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "show project root, zone slug, git, and data locations",
        argument_hint: "",
        help: "show THIS project's identity: root · zone slug · git executable · where memory/skills/sessions live (also `aizen where`, `aizen zone migrate`)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Handoff,
        name: "handoff",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "start a fresh thread carrying only what matters",
        argument_hint: "<goal>",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Goal,
        name: "goal",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "run until a goal is done (self-declared + verified)",
        argument_hint: "<text>|off",
        help: "<text>       run until the goal is done — model self-declares (goal_complete) + verify passes; no iteration cap, auto-retries API errors (incl. empty 200); /goal off to stop, Esc to cancel",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Model,
        name: "model",
        aliases: &[("models", "alias for /model")],
        hidden_aliases: &[],
        hidden: false,
        description: "list and pick the model",
        argument_hint: "",
        help: "list the provider's models (with context windows) + pick one",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Provider,
        name: "provider",
        aliases: &[],
        hidden_aliases: &["providers"],
        hidden: false,
        description: "switch, add, or manage saved endpoint profiles",
        argument_hint: "[name|add|manage]",
        help: "[name]   one-pick switch; `add` creates and `manage` edits/renames/deletes providers",
        stdin: Stdin::BareOr(&["add", "manage"]),
    },
    Builtin {
        id: SlashId::Config,
        name: "config",
        aliases: &[("setup", "alias for /config")],
        hidden_aliases: &[],
        hidden: false,
        description: "set endpoint, key, model, and provider profiles",
        argument_hint: "",
        help: "set endpoint + key + model and manage provider profiles",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Memory,
        name: "memory",
        aliases: &[("mem", "alias for /memory")],
        hidden_aliases: &[],
        hidden: false,
        description: "inspect and edit what's remembered",
        argument_hint: "[list|show|edit|forget|restore|<query>]",
        help: "[query]    show your profile, or search memory; /memory remember <fact> to save",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Persona,
        name: "persona",
        aliases: &[
            ("personas", "alias for /persona"),
            ("character", "alias for /persona"),
        ],
        hidden_aliases: &[],
        hidden: false,
        description: "pick the agent persona",
        argument_hint: "",
        help: "pick the character the agent role-plays (list · select · new · clear · delete)",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Skills,
        name: "skills",
        aliases: &[("skill", "alias for /skills")],
        hidden_aliases: &[],
        hidden: false,
        description: "browse and manage saved skills",
        argument_hint: "",
        help: "saved procedures the agent can load (list · view · new · delete)",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Commands,
        name: "commands",
        aliases: &[("cmds", "alias for /commands")],
        hidden_aliases: &[],
        hidden: false,
        description: "list custom markdown slash commands",
        argument_hint: "",
        help: "your custom slash commands — markdown macros in ~/.aizen/commands/ ($ARGUMENTS · @file · !`cmd`)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Apps,
        name: "apps",
        aliases: &[("integrations", "alias for /apps")],
        hidden_aliases: &[],
        hidden: false,
        description: "connect apps through MCP",
        argument_hint: "",
        help: "connected apps & MCP catalog — Telegram/Discord/Slack/webhook + browser sign-in apps",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Mcp,
        name: "mcp",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "show MCP lifecycle and tools",
        argument_hint: "",
        help: "MCP servers from ~/.aizen/mcp.json — lifecycle generation, health, pinned schema + tools",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Browser,
        name: "browser",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "show browser profiles and routes",
        argument_hint: "[doctor]",
        help: "browser profile/routes status (when built with --features browser)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Telegram,
        name: "telegram",
        aliases: &[("tg", "alias for /telegram")],
        hidden_aliases: &[],
        hidden: false,
        description: "configure the Telegram integration",
        argument_hint: "",
        help: "Telegram integration menu (setup · test · status · start daemon · disable)",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Serve,
        name: "serve",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "run the host bot daemon",
        argument_hint: "",
        help: "",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Sessions,
        name: "sessions",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "restore, save, or delete conversations",
        argument_hint: "",
        help: "saved conversations — restore · save · delete (autosaves into its own file each turn; newest first, labeled by project)",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Import,
        name: "import",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "resume a conversation started in another CLI (Claude Code / Codex)",
        argument_hint: "",
        help: "resume a conversation started in another CLI (Claude Code or Codex) — pick from transcripts whose cwd matches this project",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Resume,
        name: "resume",
        aliases: &[("continue", "alias for /resume")],
        hidden_aliases: &[],
        hidden: false,
        description: "reopen the last conversation with its context",
        argument_hint: "[name]",
        help: "reopen the last conversation FROM THIS PROJECT (or a named one); /handoff <goal> starts a fresh thread carrying only what that goal needs",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Workflows,
        name: "workflows",
        aliases: &[
            ("workflow", "alias for /workflows"),
            ("wf", "alias for /workflows"),
            ("agents-status", "alias for /workflows"),
        ],
        hidden_aliases: &[],
        hidden: false,
        description: "show live multi-agent activity (self-refreshing); stop one run",
        argument_hint: "[stop <#id|name>]",
        help: "multi-agent status — live task/workflow children, sub-agent slots (also /wf)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Work,
        name: "work",
        aliases: &[],
        hidden_aliases: &["worktree", "worktrees"],
        hidden: false,
        description: "isolated git worktrees, one per session",
        argument_hint: "[list|new <name>|remove <name>]",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Agents,
        name: "agents",
        aliases: &[("agent", "alias for /agents")],
        hidden_aliases: &[],
        hidden: false,
        description: "list and configure specialist agents",
        argument_hint: "",
        help: "specialist sub-agents — list · set-provider <agent> <provider> [model]",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Recover,
        name: "recover",
        aliases: &[("recovery", "alias for /recover")],
        hidden_aliases: &[],
        hidden: false,
        description: "restore a crashed session safely",
        argument_hint: "[discard]",
        help: "a session interrupted by a crash/kill — restore its transcript + unsent draft, or /recover discard",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Timemachine,
        name: "timemachine",
        aliases: &[
            ("timeline", "alias for /timemachine"),
            ("tm", "alias for /timemachine"),
        ],
        hidden_aliases: &[],
        hidden: false,
        description: "browse checkpoints and jump back to that code + chat",
        argument_hint: "",
        help: "browse every checkpoint (▸ = current) and pick one to jump back to that code + chat; also /timeline · /tm · /undo · /redo",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Checkpoint,
        name: "checkpoint",
        aliases: &[
            ("snapshot", "alias for /checkpoint"),
            ("cp", "alias for /checkpoint"),
        ],
        hidden_aliases: &[],
        hidden: false,
        description: "save a code restore point",
        argument_hint: "[note]",
        help: "save a restore point of the working tree now",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Diff,
        name: "diff",
        aliases: &[],
        hidden_aliases: &["changes"],
        hidden: false,
        description: "what changed between two points in time",
        argument_hint: "[from] [to] [-p]",
        help: "what changed in the working tree since a checkpoint (read before you /undo)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Compact,
        name: "compact",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "compress context to free tokens",
        argument_hint: "",
        help: "summarize older turns to free context now",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Lsp,
        name: "lsp",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "type-aware code navigation and diagnostics",
        argument_hint: "[on|off|status|restart]",
        help: "type-aware navigation + symbol_replace/insert + diagnostics via a language server (rust-analyzer · pyright · typescript-language-server); default ON (lazy spawn), /lsp off reclaims RAM",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Reach,
        name: "reach",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "check web-access backend health",
        argument_hint: "[doctor|status]",
        help: "web-access channels: live-probe every backend (doctor) or show what served this session (status); web_fetch/web_search route through these",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Approval,
        name: "approval",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "set the approval level",
        argument_hint: "[ask|smart|yolo]",
        help: "approval level — ask every time, auto-run read-only, or pre-authorize",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Sandbox,
        name: "sandbox",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "OS sandbox around model-run commands",
        argument_hint: "[status|auto|strict|guarded|off]",
        help: "the OS sandbox under approval: status shows what THIS machine enforces (kernel vs software); auto|strict|guarded|off sets the mode (strict fails closed where the kernel can't enforce). Full detail: `aizen sandbox status|doctor`",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Effort,
        name: "effort",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "set reasoning effort",
        argument_hint: "[auto|off|low|medium|high|xhigh|max]",
        help: "drag an animated slider (auto · low · medium · high · xhigh · max); or /effort auto|off|low|medium|high|xhigh|max|clear to set it directly",
        stdin: Stdin::WhenBare,
    },
    Builtin {
        id: SlashId::Ultimate,
        name: "ultimate",
        aliases: &[("ultra", "alias for /ultimate")],
        hidden_aliases: &[],
        hidden: false,
        description: "toggle maximum-effort orchestration mode",
        argument_hint: "",
        help: "toggle ultimate mode — max reasoning effort + prefer launching workflows (aizen's ultracode)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Clear,
        name: "clear",
        aliases: &[("new", "alias for /clear"), ("reset", "alias for /clear")],
        hidden_aliases: &[],
        hidden: false,
        description: "start a fresh conversation",
        argument_hint: "",
        help: "start a fresh conversation",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Tokens,
        name: "tokens",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "show session token usage",
        argument_hint: "",
        help: "show session token usage (context-fill HUD)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Context,
        name: "context",
        aliases: &[("ctx", "alias for /context")],
        hidden_aliases: &[],
        hidden: false,
        description: "break down context-window usage",
        argument_hint: "",
        help: "break down what fills the context window (system prompt · tool schemas · conversation by role)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Cost,
        name: "cost",
        aliases: &[("usage", "alias for /cost")],
        hidden_aliases: &[],
        hidden: false,
        description: "show session token cost",
        argument_hint: "",
        help: "session usage + $ estimate (real tokens when the provider reports them; set rates via `aizen config set --price-in/--price-out`)",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Tools,
        name: "tools",
        aliases: &[("toolsets", "alias for /tools")],
        hidden_aliases: &[],
        hidden: false,
        description: "show toolset configuration",
        argument_hint: "",
        help: "",
        stdin: Stdin::WhenArg(&["menu", "toggle"]),
    },
    Builtin {
        id: SlashId::Update,
        name: "update",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "show every aizen version and install the one you pick",
        argument_hint: "",
        help: "show the installed version next to every published one and install the one you pick (newer or older) — the new build starts in your NEXT terminal",
        stdin: Stdin::Always,
    },
    Builtin {
        id: SlashId::Undo,
        name: "undo",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "rewind to the previous checkpoint",
        argument_hint: "",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Redo,
        name: "redo",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "re-apply the next checkpoint",
        argument_hint: "",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Quit,
        name: "quit",
        aliases: &[("exit", "alias for /quit"), ("q", "alias for /quit")],
        hidden_aliases: &[],
        hidden: false,
        description: "exit aizen",
        argument_hint: "",
        help: "exit",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Save,
        name: "save",
        aliases: &[("load", "legacy alias; use /sessions")],
        hidden_aliases: &[],
        hidden: false,
        description: "legacy alias; use /sessions",
        argument_hint: "",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Smart,
        name: "smart",
        aliases: &[],
        hidden_aliases: &[],
        hidden: false,
        description: "legacy approval alias",
        argument_hint: "",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::Team,
        name: "team",
        aliases: &[],
        hidden_aliases: &["sessions-live"],
        hidden: true,
        description: "live sessions working this repo — status - claim - handoff",
        argument_hint: "[status|claim|release]",
        help: "",
        stdin: Stdin::Never,
    },
Builtin {
        id: SlashId::Yolo,
        name: "yolo",
        aliases: &[
            ("auto", "legacy approval alias"),
            ("yes", "legacy approval alias"),
        ],
        hidden_aliases: &[],
        hidden: true,
        description: "legacy shortcut for `/approval yolo`",
        argument_hint: "",
        help: "",
        stdin: Stdin::Never,
    },
    Builtin {
        id: SlashId::AutoCopy,
        name: "auto-copy",
        aliases: &[("autocopy", "alias for /auto-copy")],
        hidden_aliases: &[],
        hidden: false,
        description: "auto-copy on mouse select release",
        argument_hint: "[on|off|status]",
        help: "on (default): releasing a drag-select copies to the clipboard. off: keep the highlight and copy with Ctrl-C (Windows/Linux) or ⌘C (macOS). bare /auto-copy toggles",
        stdin: Stdin::Never,
    },
];
/// What a line beginning with `/` actually IS.
///
/// Historically every surface did `strip_prefix('/')` and treated the remainder as a command, so a
/// message that merely *starts* with a slash — an XPath (`/html/body/div[2]`), a POSIX path
/// (`/usr/bin/python`), or prose (`/help... abcd`) — was swallowed and answered with "unknown
/// command" instead of reaching the model. This type makes the decision explicit and shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Dispatch `name` with `arg` (both already trimmed, `name` lowercased).
    Command { name: String, arg: String },
    /// Close to a real command but not one. Show the suggestion; do NOT dispatch and do NOT send to
    /// the model — a typo'd `/claer` must never silently run `/clear`.
    DidYouMean { typed: String, best: String },
    /// Not a command. Send the ORIGINAL line (leading `/` intact) to the model verbatim.
    Chat,
}

impl Verdict {
    /// Downgrade to [`Verdict::Chat`] unless `keep` holds.
    ///
    /// The retained input box uses this for vision messages: a line submitted WITH an image
    /// attachment is a message to the model, never a command, whatever it happens to start with.
    pub fn filter_command(self, keep: bool) -> Self {
        if keep {
            self
        } else {
            Verdict::Chat
        }
    }
}

/// Minimum token length before fuzzy matching is attempted.
///
/// Jaro-Winkler's prefix bonus inflates short strings: `ls`→`lsp` scores 0.911 and `cd`→`cmds`
/// 0.850, which would hijack two-letter words that are obviously prose. Measured over the full
/// 75-command catalog, real typos (`hepl`→`help` 0.933, `memmory`→`memory` 0.967) are all ≥3 chars.
const FUZZY_MIN_LEN: usize = 3;

/// Jaro-Winkler floor for "did you mean".
///
/// Chosen from measurement, not taste. Real typos cluster at 0.90–0.97 (`sesions`→`sessions` 0.904,
/// `modle`→`model` 0.953); non-commands cluster well below (`abcd`→`handoff` 0.595, `build`→`quit`
/// 0.633, `npm`→`snapshot` 0.639). The gap around 0.90 is wide and empty.
const FUZZY_MIN_SCORE: f64 = 0.90;

/// Maximum length difference between the typed token and a candidate.
///
/// A transposition or a doubled letter changes length by at most 1–2. Without this, a short token
/// can score above the floor against a much longer command purely on its prefix.
const FUZZY_MAX_LEN_DELTA: usize = 2;

/// Longest plausible command name — anything longer is prose or a path, not a typo.
const MAX_NAME_LEN: usize = 32;

/// Everything the `/help` page says that is not a command row.
///
/// Kept verbatim: these describe input affordances (`#remember`, `!shell`, `@file`) rather than
/// commands, so there is no row for them to hang off.
const INPUT_SHORTCUTS: &str = "\
Input shortcuts (in a normal message):
  #<text>            remember <text> as a durable fact (one keystroke into the memory brain) — sends no turn
  !<cmd>             run <cmd> in the shell and show output (the safety floor still blocks catastrophic commands) — sends no turn
  @<path>            inline a file's contents into your message
  !`<cmd>`           splice a read-only command's output into your message
Anything else you type goes to the agent (it chats and uses tools in one loop).";

/// Column the description starts in, so the page reads as a table.
const HELP_TEXT_COLUMN: usize = 21;

/// Render the `/help` page from [`BUILTINS`].
///
/// This was a hand-written const, and it showed: `/where` was listed twice with two different
/// descriptions, while `/handoff`, `/save`, `/serve`, `/smart`, `/tools`, `/undo`, `/redo` and
/// `/work` all worked and were documented nowhere. Generating it means a command that exists is a
/// command that is listed — there is no second copy left to forget.
pub fn help_page() -> String {
    let mut out = String::from("Commands:\n");
    for b in BUILTINS.iter().filter(|b| !b.hidden) {
        let label = if b.argument_hint.is_empty() {
            format!("/{}", b.name)
        } else {
            format!("/{} {}", b.name, b.argument_hint)
        };
        let text = if b.help.is_empty() {
            b.description
        } else {
            b.help
        };
        let pad = HELP_TEXT_COLUMN.saturating_sub(2 + label.len()).max(1);
        out.push_str(&format!("  {label}{blank:pad$}{text}\n", blank = ""));
    }
    out.push('\n');
    out.push_str(INPUT_SHORTCUTS);
    out
}

/// The built-in a spelling resolves to — canonical name, listed alias, or hidden alias alike.
///
/// The single entry point from a typed token to a command's identity. Everything downstream
/// (dispatch, stdin suspension, the prose gate) keys off the returned row, so a name can never be
/// accepted by one surface and unknown to another.
pub fn resolve(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| {
        b.name == name
            || b.aliases.iter().any(|(a, _)| *a == name)
            || b.hidden_aliases.contains(&name)
    })
}

/// Whether the command line `input` opens something that takes over stdin (a `dialoguer` menu, the
/// effort slider, a daemon), so the retained frame must suspend before running it.
///
/// Takes the FULL line because the answer can depend on the argument — bare `/effort` drags a
/// slider while `/effort high` just sets it. Unknown names are not ours to suspend for.
pub fn takes_stdin(input: &str) -> bool {
    let mut parts = input.trim().splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    resolve(name).is_some_and(|b| b.stdin.claims(arg))
}

/// Whether `name` is a command the dispatcher will actually handle (catalog + hidden aliases).
fn is_known(name: &str) -> bool {
    resolve(name).is_some() || list().iter().any(|c| c.name == name)
}

/// Whether a token could be a command NAME at all, on shape alone.
///
/// Must start with a letter and contain only `[A-Za-z0-9_:-]`. This single rule rejects every
/// false-positive class we've actually hit: `help...` (dot), `html/body/div[2]` (slash, bracket),
/// `c/Users/admin` (slash), `usr/bin/python` (slash). It is deterministic — no guessing.
///
/// Public because the host bot needs it directly: that surface SHELLS OUT on an unrecognized
/// command name, so it must distinguish "unknown command" (shape-valid → run it) from "not a
/// command at all" (a path or prose → hand to the agent).
pub fn looks_like_name(tok: &str) -> bool {
    !tok.is_empty()
        && tok.len() <= MAX_NAME_LEN
        && tok.starts_with(|c: char| c.is_ascii_alphabetic())
        && tok
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
}

/// Whether `name` accepts arguments, per its catalog `argument_hint`.
///
/// Drives the prose gate below: `/clear now` still clears (one stray word), but
/// `/model của aizen là gì?` is a question about the model, not a request to open the picker.
fn takes_args(name: &str) -> bool {
    // An alias resolves to its command's row, so it inherits the behaviour automatically — this
    // used to be a hand-written mirror list that only covered four of them.
    if let Some(b) = resolve(name) {
        return !b.argument_hint.trim().is_empty();
    }
    list()
        .iter()
        .find(|c| c.name == name)
        // A custom command always takes args ($ARGUMENTS), whether or not it declares a hint.
        .map(|c| c.custom || !c.argument_hint.trim().is_empty())
        .unwrap_or(false)
}

/// Words of trailing prose tolerated after a no-argument command before it reads as a sentence.
///
/// `/clear now` = 1 → still a command. `/model của aizen là gì?` = 4 → a question.
const PROSE_WORD_TOLERANCE: usize = 1;

/// Decide whether a submitted line is a slash command, a near-miss, or ordinary chat.
///
/// `line` is the raw input INCLUDING the leading `/`. Rules are applied in order; the first match
/// wins. Every surface that dispatches slash commands (retained REPL, plain REPL, host bot) calls
/// this so the three cannot drift apart — they previously each open-coded `strip_prefix('/')`.
pub fn classify(line: &str) -> Verdict {
    let Some(rest) = line.strip_prefix('/') else {
        return Verdict::Chat;
    };

    // 1. `/ hello` — a slash followed by whitespace is never a command. Guarded explicitly because
    //    `splitn` would yield an EMPTY name here, which `handle_slash`'s `"help" | "?" | ""` arm
    //    matches: typing "/ hello" used to print the help page.
    if rest.starts_with(char::is_whitespace) || rest.trim().is_empty() {
        return Verdict::Chat;
    }

    let (tok, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let name = tok.to_ascii_lowercase();

    // 2. Exact hit, checked BEFORE the shape gate so a punctuation-only alias (`/?`) survives it.
    //    Safe to hoist: a path or punctuated word is never an exact command name, so nothing that
    //    the shape gate is meant to reject can reach this branch.
    if is_known(&name) {
        // 2a. Prose gate: a command that takes NO arguments, followed by a sentence, is a question
        //     about that command rather than an invocation of it.
        if !takes_args(&name) && arg.split_whitespace().count() > PROSE_WORD_TOLERANCE {
            return Verdict::Chat;
        }
        return Verdict::Command {
            name,
            arg: arg.to_string(),
        };
    }

    // 3. Shape gate — the cheap, deterministic rule that catches paths and punctuated prose. Only
    //    reached for tokens that are NOT commands, so it can be strict without breaking aliases.
    //
    //    ORDER MATTERS: this must stay ABOVE the fuzzy step. A punctuated near-miss like `/model?`
    //    scores 0.976 against `model`, well over the floor, so with the two steps swapped it would
    //    answer "did you mean /model?" instead of asking the model the question. Relaxing this gate
    //    therefore silently re-breaks the punctuated-prose case even though the fuzzy thresholds
    //    are untouched — `punctuated_prose_after_a_command_name_is_chat` pins that coupling.
    if !looks_like_name(tok) {
        return Verdict::Chat;
    }

    // 4. Near-miss → suggest, never auto-run (a typo'd `/claer` must not wipe the conversation).
    if name.len() >= FUZZY_MIN_LEN {
        // Suggest from every spelling that would actually dispatch — listed or not. A hidden alias
        // is still a real command, so `/yol` should offer `/yolo` rather than nothing.
        let best = list()
            .into_iter()
            .map(|c| c.name)
            .chain(
                BUILTINS
                    .iter()
                    .flat_map(|b| b.hidden_aliases.iter().copied().chain([b.name]))
                    .map(str::to_string),
            )
            .filter(|cand| cand.len().abs_diff(name.len()) <= FUZZY_MAX_LEN_DELTA)
            .map(|cand| {
                let score = strsim::jaro_winkler(&name, &cand);
                (cand, score)
            })
            .filter(|(_, score)| *score >= FUZZY_MIN_SCORE)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((best, _)) = best {
            return Verdict::DidYouMean { typed: name, best };
        }
    }

    // 5. Unrecognized and not close to anything → the user meant it as text.
    Verdict::Chat
}

/// Return built-ins followed by project/global custom commands.
///
/// Built-ins win a name collision because `handle_slash` dispatches them before falling back to
/// `commands::find`; hiding the custom row avoids presenting a command that cannot be invoked.
pub fn list() -> Vec<SlashCommand> {
    // Two blocks, as the catalog has always shown them: the commands themselves, then the alias
    // spellings kept for muscle memory. Both are derived from the one table, so an alias row can no
    // longer outlive the command it points at.
    let row = |name: &str, c: &'static Builtin, desc: String| SlashCommand {
        name: name.to_string(),
        description: desc,
        argument_hint: c.argument_hint.to_string(),
        custom: false,
    };
    let mut out: Vec<SlashCommand> = BUILTINS
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| row(c.name, c, c.description.to_string()))
        .collect();
    out.extend(BUILTINS.iter().flat_map(|c| {
        c.aliases
            .iter()
            .map(move |(a, note)| row(a, c, note.to_string()))
    }));
    let builtin_names: std::collections::HashSet<&str> = BUILTINS
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().map(|(a, _)| *a)))
        .collect();
    out.extend(
        commands::list()
            .into_iter()
            .filter(|c| !builtin_names.contains(c.name.as_str()))
            .map(|c| SlashCommand {
                name: c.name,
                description: if c.description.trim().is_empty() {
                    "custom command".to_string()
                } else {
                    c.description
                },
                argument_hint: c.argument_hint,
                custom: true,
            }),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_unique_and_described() {
        let mut names = std::collections::HashSet::new();
        for c in BUILTINS {
            assert!(names.insert(c.name), "duplicate slash command /{}", c.name);
            assert!(
                !c.description.trim().is_empty(),
                "missing description for /{}",
                c.name
            );
        }
    }

    #[test]
fn init_and_currently_omitted_commands_are_catalogued() {
        let names: std::collections::HashSet<String> = list().into_iter().map(|c| c.name).collect();
        for name in [
            "init",
            "handoff",
            "goal",
            "lsp",
            "reach",
            "agents",
            "tools",
            "browser",
            "undo",
            "redo",
            "serve",
            "auto-copy",
            "autocopy",
        ] {
            assert!(names.contains(name), "slash catalog must include /{name}");
        }
        assert_eq!(
            resolve("auto-copy").map(|b| b.id),
            Some(SlashId::AutoCopy)
        );
        assert_eq!(resolve("autocopy").map(|b| b.id), Some(SlashId::AutoCopy));
        assert_eq!(
            cmd("/auto-copy off"),
            ("auto-copy".into(), "off".into())
        );
    }

    // ── classify ────────────────────────────────────────────────────────────────────────────

    fn cmd(line: &str) -> (String, String) {
        match classify(line) {
            Verdict::Command { name, arg } => (name, arg),
            other => panic!("expected Command for {line:?}, got {other:?}"),
        }
    }

    #[test]
    fn plain_commands_still_dispatch() {
        assert_eq!(cmd("/help"), ("help".into(), "".into()));
        assert_eq!(cmd("/model"), ("model".into(), "".into()));
        // Arg-taking commands keep their whole argument string.
        assert_eq!(
            cmd("/goal ship the release"),
            ("goal".into(), "ship the release".into())
        );
        assert_eq!(cmd("/init --force"), ("init".into(), "--force".into()));
        assert_eq!(cmd("/effort max"), ("effort".into(), "max".into()));
    }

    #[test]
    fn every_spelling_the_table_lists_is_dispatchable() {
        // The invariant the old design could not hold. `/team` and `/providers` each had a working
        // arm in `handle_slash` while `classify` had never heard of them, so typing either one was
        // answered with "did you mean…" or sent to the model as prose. With one table there is no
        // second list to fall out of — this test pins that.
        for b in BUILTINS {
            for spelling in std::iter::once(b.name)
                .chain(b.aliases.iter().map(|(a, _)| *a))
                .chain(b.hidden_aliases.iter().copied())
            {
                let line = format!("/{spelling}");
                assert!(
                    matches!(classify(&line), Verdict::Command { .. }),
                    "/{spelling} is in the table but classify() will not dispatch it"
                );
                assert_eq!(
                    resolve(spelling).map(|r| r.id),
                    Some(b.id),
                    "/{spelling} resolves to the wrong command"
                );
            }
        }
    }

    #[test]
    fn the_help_page_documents_every_visible_command_exactly_once() {
        // The hand-written page had drifted both ways at once: /where twice, eight working commands
        // absent. Neither is expressible now, and this test says so out loud.
        let page = help_page();
        for b in BUILTINS {
            let hits = page
                .lines()
                .filter(|l| {
                    l.starts_with(&format!("  /{} ", b.name))
                        || l.trim_end() == format!("  /{}", b.name)
                })
                .count();
            assert_eq!(
                hits,
                usize::from(!b.hidden),
                "/{} appears {hits} times on the help page",
                b.name
            );
        }
    }

    #[test]
    fn a_command_without_curated_help_still_gets_a_line() {
        // `help: ""` must fall back to `description` rather than print an empty column.
        for b in BUILTINS.iter().filter(|b| !b.hidden && b.help.is_empty()) {
            assert!(
                help_page().contains(b.description),
                "/{} has no help text and no description on the page",
                b.name
            );
        }
    }

    #[test]
    fn no_spelling_is_claimed_by_two_commands() {
        let mut seen = std::collections::HashMap::new();
        for b in BUILTINS {
            for spelling in std::iter::once(b.name)
                .chain(b.aliases.iter().map(|(a, _)| *a))
                .chain(b.hidden_aliases.iter().copied())
            {
                if let Some(prev) = seen.insert(spelling, b.name) {
                    panic!("/{spelling} is claimed by both /{prev} and /{}", b.name);
                }
            }
        }
    }

    #[test]
    fn aliases_inherit_the_argument_behaviour_of_their_command() {
        // Used to be a hand-written mirror list covering four aliases; the other twenty-seven were
        // silently wrong, which fed the prose gate the wrong answer.
        for b in BUILTINS {
            for spelling in b
                .aliases
                .iter()
                .map(|(a, _)| *a)
                .chain(b.hidden_aliases.iter().copied())
            {
                assert_eq!(
                    takes_args(spelling),
                    takes_args(b.name),
                    "/{spelling} disagrees with /{} about taking arguments",
                    b.name
                );
            }
        }
    }

    #[test]
    fn stdin_ownership_matches_the_rule_it_replaced() {
        // Transcribed from the `matches!` block that used to live in `ui::tui::slash_takes_stdin`.
        // If a row's `stdin:` field is edited by accident, the retained frame either paints over a
        // dialoguer menu or suspends for a command that only prints — both were live bugs once.
        for line in [
            "config",
            "setup",
            "persona",
            "personas",
            "character",
            "skills",
            "skill",
            "apps",
            "integrations",
            "telegram",
            "tg",
            "serve",
            "sessions",
            "import",
            "model",
            "provider",
            "provider add",
            "provider manage",
            "timemachine",
            "timeline",
            "tm",
            "effort",
            "update",
            "tools menu",
            "toolsets toggle",
        ] {
            assert!(takes_stdin(line), "/{line} must suspend the retained frame");
        }
        for line in [
            "help",
            "where",
            "tokens",
            "cost",
            "mcp",
            "memory",
            "diff",
            "effort high",
            "provider openrouter",
            "tools",
            "toolsets",
            "compact",
            "undo",
            "redo",
        ] {
            assert!(!takes_stdin(line), "/{line} must run with the box still up");
        }
    }

    #[test]
    fn punctuated_prose_after_a_command_name_is_chat() {
        // The reported bug: `/help... abcd` was swallowed with "unknown command" instead of asked.
        assert_eq!(classify("/help... abcd"), Verdict::Chat);
        assert_eq!(classify("/model?"), Verdict::Chat);
        assert_eq!(classify("/clear!!"), Verdict::Chat);
    }

    #[test]
    fn paths_and_xpaths_are_chat_not_commands() {
        for line in [
            "/html/body/div[2]/span",         // XPath — the "copy full xpath" report
            "/usr/bin/python có gì",          // POSIX absolute path
            "/c/Users/admin/Desktop/foo.txt", // git-bash style Windows path
            "/etc/hosts",
            "//div[@id='root']", // XPath double-slash
        ] {
            assert_eq!(classify(line), Verdict::Chat, "{line} must reach the model");
        }
    }

    #[test]
    fn slash_then_space_does_not_open_help() {
        // `/ hello` produced an EMPTY command name, which handle_slash's `"help" | "?" | ""` arm
        // matched — so a message beginning with a lone slash printed the help page.
        assert_eq!(classify("/ hello"), Verdict::Chat);
        assert_eq!(classify("/ "), Verdict::Chat);
        assert_eq!(classify("/"), Verdict::Chat);
    }

    #[test]
    fn typos_suggest_but_never_auto_run() {
        for (typed, want) in [
            ("/modle", "model"),
            ("/hepl", "help"),
            ("/claer", "clear"),
            ("/memmory", "memory"),
            ("/sesions", "sessions"),
        ] {
            match classify(typed) {
                Verdict::DidYouMean { best, .. } => assert_eq!(best, want, "for {typed}"),
                other => panic!("{typed} should suggest /{want}, got {other:?}"),
            }
        }
    }

    #[test]
    fn destructive_typo_is_never_dispatched() {
        // The reason DidYouMean exists rather than auto-correct: `/claer` resolving to `/clear`
        // would silently wipe the conversation on a keystroke slip.
        assert!(
            !matches!(classify("/claer"), Verdict::Command { .. }),
            "a typo must never dispatch a destructive command"
        );
    }

    #[test]
    fn short_tokens_do_not_fuzzy_match() {
        // Jaro-Winkler's prefix bonus scores `ls`→`lsp` at 0.911 and `cd`→`cmds` at 0.850. Without
        // FUZZY_MIN_LEN those become bogus suggestions for obvious prose.
        for line in ["/ls", "/cd", "/c", "/x"] {
            assert_eq!(
                classify(line),
                Verdict::Chat,
                "{line} is too short to guess"
            );
        }
    }

    #[test]
    fn unrelated_words_are_chat() {
        for line in ["/abcd", "/xyzzy", "/foo", "/build", "/npm", "/deploy"] {
            assert_eq!(classify(line), Verdict::Chat, "{line} matches nothing");
        }
    }

    #[test]
    fn no_arg_command_with_a_sentence_after_it_is_a_question() {
        // `/model` takes no arguments, so a sentence after it is prose about the command.
        assert_eq!(classify("/model của aizen là gì?"), Verdict::Chat);
        assert_eq!(classify("/compact làm ngay đi"), Verdict::Chat);
        // …but one stray word is tolerated, so muscle memory keeps working.
        assert_eq!(cmd("/clear now"), ("clear".into(), "now".into()));
    }

    #[test]
    fn arg_taking_commands_accept_long_arguments() {
        // The prose gate must NOT fire for commands whose whole point is a free-text argument.
        assert_eq!(
            cmd("/goal làm cho xong bản release rồi báo tôi"),
            ("goal".into(), "làm cho xong bản release rồi báo tôi".into())
        );
        assert!(matches!(
            classify("/handoff finish the retry work"),
            Verdict::Command { .. }
        ));
        assert!(matches!(
            classify("/memory what did I say about MCP"),
            Verdict::Command { .. }
        ));
    }

    #[test]
    fn command_names_are_case_insensitive() {
        assert_eq!(cmd("/HELP").0, "help");
        assert_eq!(cmd("/Model").0, "model");
    }

    // ── inverse checks: prove the thresholds actually discriminate ───────────────────────────

    #[test]
    fn fuzzy_thresholds_separate_typos_from_prose() {
        // If FUZZY_MIN_SCORE were lowered to ~0.6, `/abcd` would "suggest" /handoff (0.595) and
        // `/build` would suggest /quit (0.633). Pin the actual gap so a future tweak that erases it
        // fails here instead of in the user's terminal.
        let score = |a: &str, b: &str| strsim::jaro_winkler(a, b);
        // Real typos sit above the floor.
        assert!(score("modle", "model") >= FUZZY_MIN_SCORE);
        assert!(score("memmory", "memory") >= FUZZY_MIN_SCORE);
        assert!(score("sesions", "sessions") >= FUZZY_MIN_SCORE);
        // Non-commands sit far below it.
        assert!(score("abcd", "handoff") < FUZZY_MIN_SCORE);
        assert!(score("build", "quit") < FUZZY_MIN_SCORE);
        assert!(score("npm", "snapshot") < FUZZY_MIN_SCORE);
    }

    #[test]
    fn shape_gate_is_what_rejects_paths() {
        // Directly pin the mechanism, not just its effect: if `looks_like_name` were relaxed to
        // allow `/` or `.`, every path test above would regress at once.
        assert!(looks_like_name("help"));
        assert!(looks_like_name("agents-status"));
        assert!(looks_like_name("git:commit")); // namespaced custom command
        assert!(!looks_like_name("help..."));
        assert!(!looks_like_name("html/body/div[2]"));
        assert!(!looks_like_name("usr/bin/python"));
        assert!(!looks_like_name("2fast")); // must start with a letter
        assert!(!looks_like_name(&"a".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn the_catalog_shows_every_command_and_every_listed_alias_once() {
        // `list()` is what the `/` picker and the live palette render. A hidden row is deliberate
        // (a superseded spelling); anything else missing means a command exists that no surface
        // will ever show.
        let shown: Vec<String> = list()
            .into_iter()
            .filter(|c| !c.custom)
            .map(|c| c.name)
            .collect();
        for b in BUILTINS {
            assert_eq!(
                shown.iter().filter(|n| n.as_str() == b.name).count(),
                usize::from(!b.hidden),
                "/{} is listed the wrong number of times",
                b.name
            );
            for (a, _) in b.aliases {
                assert_eq!(
                    shown.iter().filter(|n| n == a).count(),
                    1,
                    "/{a} should appear exactly once in the catalog"
                );
            }
        }
    }

    #[test]
    fn host_bot_vocabulary_is_not_in_this_catalog() {
        // WHY `hostbot::daemon` calls `looks_like_name` instead of `classify`.
        //
        // The bot has its own command set, and its dispatcher deliberately runs an UNRECOGNIZED
        // name as a shell command. Routing it through `classify` would classify each of these as
        // prose and hand it to the agent as chat — silently killing remote shell control. This test
        // fails the moment someone "unifies" the two vocabularies without also revisiting that
        // decision.
        let catalog: std::collections::HashSet<String> =
            list().into_iter().map(|c| c.name).collect();
        for bot_only in ["sh", "pwd", "bots", "addbot", "rmbot", "start"] {
            assert!(
                !catalog.contains(bot_only),
                "/{bot_only} is host-bot-only; if it is now a REPL command, revisit daemon.rs's \
                 shape-gate-only dispatch"
            );
        }
        // `cd` is likewise bot-only, and short enough that fuzzy matching would mangle it.
        assert!(!catalog.contains("cd"));
    }

    #[test]
    fn a_shape_valid_unknown_name_still_reaches_the_host_bots_shell_fallback() {
        // The bot gate is `looks_like_name` alone. Pin both directions: a plausible command name
        // passes (so `/ls` can still shell out remotely), a path does not (the reported bug).
        assert!(looks_like_name("ls"));
        assert!(looks_like_name("docker"));
        assert!(!looks_like_name("usr/bin/python"));
        assert!(!looks_like_name("c/Users/admin/Desktop"));
    }
}
