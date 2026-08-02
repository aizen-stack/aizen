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

struct Builtin {
    name: &'static str,
    description: &'static str,
    argument_hint: &'static str,
}

/// Every slash token accepted by `handle_slash`.
///
/// Keep the canonical command before its compatibility aliases. The test below checks that this
/// table is unique; the UI and picker both consume this exact list rather than maintaining copies.
const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "help",
        description: "show commands and tips",
        argument_hint: "",
    },
    Builtin {
        name: "init",
        description: "index the codebase for semantic search + auto-retrieval",
        argument_hint: "[--force|--status]",
    },
    Builtin {
        name: "where",
        description: "show project root, zone slug, git, and data locations",
        argument_hint: "",
    },
    Builtin {
        name: "handoff",
        description: "start a fresh thread carrying only what matters",
        argument_hint: "<goal>",
    },
    Builtin {
        name: "goal",
        description: "run until a goal is done (self-declared + verified)",
        argument_hint: "<text>|off",
    },
    Builtin {
        name: "model",
        description: "list and pick the model",
        argument_hint: "",
    },
    Builtin {
        name: "config",
        description: "set endpoint, key, and model",
        argument_hint: "",
    },
    Builtin {
        name: "memory",
        description: "inspect and edit what's remembered",
        argument_hint: "[list|show|edit|forget|restore|<query>]",
    },
    Builtin {
        name: "persona",
        description: "pick the agent persona",
        argument_hint: "",
    },
    Builtin {
        name: "skills",
        description: "browse and manage saved skills",
        argument_hint: "",
    },
    Builtin {
        name: "commands",
        description: "list custom markdown slash commands",
        argument_hint: "",
    },
    Builtin {
        name: "apps",
        description: "connect apps through MCP",
        argument_hint: "",
    },
    Builtin {
        name: "mcp",
        description: "show MCP lifecycle and tools",
        argument_hint: "",
    },
    Builtin {
        name: "browser",
        description: "show browser profiles and routes",
        argument_hint: "[doctor]",
    },
    Builtin {
        name: "telegram",
        description: "configure the Telegram integration",
        argument_hint: "",
    },
    Builtin {
        name: "serve",
        description: "run the host bot daemon",
        argument_hint: "",
    },
    Builtin {
        name: "sessions",
        description: "restore, save, or delete conversations",
        argument_hint: "",
    },
    Builtin {
        name: "import",
        description: "resume a conversation started in another CLI (Claude Code / Codex)",
        argument_hint: "",
    },
    Builtin {
        name: "resume",
        description: "reopen the last conversation with its context",
        argument_hint: "[name]",
    },
    Builtin {
        name: "workflows",
        description: "show live multi-agent activity (self-refreshing); stop one run",
        argument_hint: "[stop <#id|name>]",
    },
    Builtin {
        name: "team",
        description: "see other aizen windows in this repo, their files, diffs, and commit their work",
        argument_hint: "[status|diff <s>|claims|task <text>|done|commit <s>]",
    },
    Builtin {
        name: "work",
        description: "isolated git worktrees, one per session",
        argument_hint: "[list|new <name>|remove <name>]",
    },
    Builtin {
        name: "agents",
        description: "list and configure specialist agents",
        argument_hint: "",
    },
    Builtin {
        name: "recover",
        description: "restore a crashed session safely",
        argument_hint: "[discard]",
    },
    Builtin {
        name: "timemachine",
        description: "browse checkpoints and jump back to that code + chat",
        argument_hint: "",
    },
    Builtin {
        name: "checkpoint",
        description: "save a code restore point",
        argument_hint: "[note]",
    },
    Builtin {
        name: "diff",
        description: "what changed between two points in time",
        argument_hint: "[from] [to] [-p]",
    },
    Builtin {
        name: "compact",
        description: "compress context to free tokens",
        argument_hint: "",
    },
    Builtin {
        name: "lsp",
        description: "type-aware code navigation and diagnostics",
        argument_hint: "[on|off|status|restart]",
    },
    Builtin {
        name: "reach",
        description: "check web-access backend health",
        argument_hint: "[doctor|status]",
    },
    Builtin {
        name: "approval",
        description: "set the approval level",
        argument_hint: "[ask|smart|yolo]",
    },
    Builtin {
        name: "effort",
        description: "set reasoning effort",
        argument_hint: "[auto|off|low|medium|high|xhigh|max]",
    },
    Builtin {
        name: "ultimate",
        description: "toggle maximum-effort orchestration mode",
        argument_hint: "",
    },
    Builtin {
        name: "clear",
        description: "start a fresh conversation",
        argument_hint: "",
    },
    Builtin {
        name: "tokens",
        description: "show session token usage",
        argument_hint: "",
    },
    Builtin {
        name: "context",
        description: "break down context-window usage",
        argument_hint: "",
    },
    Builtin {
        name: "cost",
        description: "show session token cost",
        argument_hint: "",
    },
    Builtin {
        name: "tools",
        description: "show toolset configuration",
        argument_hint: "",
    },
    Builtin {
        name: "update",
        description: "show every aizen version and install the one you pick",
        argument_hint: "",
    },
    Builtin {
        name: "undo",
        description: "rewind to the previous checkpoint",
        argument_hint: "",
    },
    Builtin {
        name: "redo",
        description: "re-apply the next checkpoint",
        argument_hint: "",
    },
    Builtin {
        name: "quit",
        description: "exit aizen",
        argument_hint: "",
    },
    Builtin {
        name: "exit",
        description: "alias for /quit",
        argument_hint: "",
    },
    Builtin {
        name: "q",
        description: "alias for /quit",
        argument_hint: "",
    },
    Builtin {
        name: "index",
        description: "alias for /init",
        argument_hint: "[--force|--status]",
    },
    Builtin {
        name: "new",
        description: "alias for /clear",
        argument_hint: "",
    },
    Builtin {
        name: "reset",
        description: "alias for /clear",
        argument_hint: "",
    },
    Builtin {
        name: "ctx",
        description: "alias for /context",
        argument_hint: "",
    },
    Builtin {
        name: "usage",
        description: "alias for /cost",
        argument_hint: "",
    },
    Builtin {
        name: "continue",
        description: "alias for /resume",
        argument_hint: "[name]",
    },
    Builtin {
        name: "save",
        description: "legacy alias; use /sessions",
        argument_hint: "",
    },
    Builtin {
        name: "load",
        description: "legacy alias; use /sessions",
        argument_hint: "",
    },
    Builtin {
        name: "workflow",
        description: "alias for /workflows",
        argument_hint: "",
    },
    Builtin {
        name: "wf",
        description: "alias for /workflows",
        argument_hint: "",
    },
    Builtin {
        name: "agents-status",
        description: "alias for /workflows",
        argument_hint: "",
    },
    Builtin {
        name: "agent",
        description: "alias for /agents",
        argument_hint: "",
    },
    Builtin {
        name: "recovery",
        description: "alias for /recover",
        argument_hint: "",
    },
    Builtin {
        name: "ultra",
        description: "alias for /ultimate",
        argument_hint: "",
    },
    Builtin {
        name: "auto",
        description: "legacy approval alias",
        argument_hint: "",
    },
    Builtin {
        name: "yes",
        description: "legacy approval alias",
        argument_hint: "",
    },
    Builtin {
        name: "smart",
        description: "legacy approval alias",
        argument_hint: "",
    },
    Builtin {
        name: "models",
        description: "alias for /model",
        argument_hint: "",
    },
    Builtin {
        name: "setup",
        description: "alias for /config",
        argument_hint: "",
    },
    Builtin {
        name: "mem",
        description: "alias for /memory",
        argument_hint: "[query]",
    },
    Builtin {
        name: "personas",
        description: "alias for /persona",
        argument_hint: "",
    },
    Builtin {
        name: "character",
        description: "alias for /persona",
        argument_hint: "",
    },
    Builtin {
        name: "skill",
        description: "alias for /skills",
        argument_hint: "",
    },
    Builtin {
        name: "integrations",
        description: "alias for /apps",
        argument_hint: "",
    },
    Builtin {
        name: "tg",
        description: "alias for /telegram",
        argument_hint: "",
    },
    Builtin {
        name: "toolsets",
        description: "alias for /tools",
        argument_hint: "",
    },
    Builtin {
        name: "cmds",
        description: "alias for /commands",
        argument_hint: "",
    },
    Builtin {
        name: "timeline",
        description: "alias for /timemachine",
        argument_hint: "",
    },
    Builtin {
        name: "tm",
        description: "alias for /timemachine",
        argument_hint: "",
    },
    Builtin {
        name: "snapshot",
        description: "alias for /checkpoint",
        argument_hint: "[note]",
    },
    Builtin {
        name: "cp",
        description: "alias for /checkpoint",
        argument_hint: "[note]",
    },
];

/// Dispatch-only aliases: names `handle_slash` accepts but that are deliberately NOT in [`BUILTINS`]
/// (they are legacy spellings or internal synonyms kept working for muscle memory, without earning a
/// row in the palette / `/help`).
///
/// [`classify`] must consult this table too. Building the "known" set from [`list`] alone would
/// classify `/yolo` as prose and silently stop dispatching a command that works today — the exact
/// regression this constant exists to prevent. Keep in sync with the match arms in `handle_slash`.
const DISPATCH_ALIASES: &[&str] = &[
    "?",
    "yolo",
    "changes",
    "worktree",
    "worktrees",
    "sessions-live",
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

/// Whether `name` is a command the dispatcher will actually handle (catalog + hidden aliases).
fn is_known(name: &str) -> bool {
    DISPATCH_ALIASES.contains(&name) || list().iter().any(|c| c.name == name)
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
    // Hidden aliases mirror the arg-taking behaviour of the command they alias.
    match name {
        "changes" => return true,          // alias of /diff
        "worktree" | "worktrees" => return true, // aliases of /work
        "sessions-live" => return true,    // alias of /team
        _ => {}
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
        let best = list()
            .into_iter()
            .map(|c| c.name)
            .chain(DISPATCH_ALIASES.iter().map(|s| s.to_string()))
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
    let mut out: Vec<SlashCommand> = BUILTINS
        .iter()
        .map(|c| SlashCommand {
            name: c.name.to_string(),
            description: c.description.to_string(),
            argument_hint: c.argument_hint.to_string(),
            custom: false,
        })
        .collect();
    let builtin_names: std::collections::HashSet<&str> = BUILTINS.iter().map(|c| c.name).collect();
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
            "init", "handoff", "goal", "lsp", "reach", "agents", "tools", "browser", "undo",
            "redo", "serve",
        ] {
            assert!(names.contains(name), "slash catalog must include /{name}");
        }
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
        assert_eq!(cmd("/goal ship the release"), ("goal".into(), "ship the release".into()));
        assert_eq!(cmd("/init --force"), ("init".into(), "--force".into()));
        assert_eq!(cmd("/effort max"), ("effort".into(), "max".into()));
    }

    #[test]
    fn hidden_dispatch_aliases_are_not_reclassified_as_chat() {
        // These work in `handle_slash` but are absent from BUILTINS on purpose. Deriving the known
        // set from `list()` alone would break them — that is what DISPATCH_ALIASES exists for.
        for alias in DISPATCH_ALIASES {
            let line = format!("/{alias}");
            assert!(
                matches!(classify(&line), Verdict::Command { .. }),
                "/{alias} must stay dispatchable"
            );
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
            "/html/body/div[2]/span",          // XPath — the "copy full xpath" report
            "/usr/bin/python có gì",           // POSIX absolute path
            "/c/Users/admin/Desktop/foo.txt",  // git-bash style Windows path
            "/etc/hosts",
            "//div[@id='root']",               // XPath double-slash
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
            assert_eq!(classify(line), Verdict::Chat, "{line} is too short to guess");
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
        assert!(matches!(classify("/handoff finish the retry work"), Verdict::Command { .. }));
        assert!(matches!(classify("/memory what did I say about MCP"), Verdict::Command { .. }));
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
    fn every_dispatch_alias_is_absent_from_builtins() {
        // DISPATCH_ALIASES is only correct while these stay OUT of BUILTINS; if one is promoted to
        // a catalogued command, it must be removed here or `is_known` double-counts it.
        let builtins: std::collections::HashSet<&str> = BUILTINS.iter().map(|c| c.name).collect();
        for alias in DISPATCH_ALIASES {
            assert!(
                !builtins.contains(alias),
                "/{alias} is now catalogued — drop it from DISPATCH_ALIASES"
            );
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
