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
        name: "resume",
        description: "reopen the last conversation with its context",
        argument_hint: "[name]",
    },
    Builtin {
        name: "workflows",
        description: "show live multi-agent activity",
        argument_hint: "",
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
}
