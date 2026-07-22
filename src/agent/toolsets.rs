//! Hermes-style **tool bundles** (`platform_toolsets.cli`): group built-in tools by capability so
//! users can shrink the model's tool schema (`disabled_toolsets` / `enabled_toolsets` in
//! `cli-config.json`) without forking the binary. MCP tools map to the `mcp` bundle; everything
//! else is classified by tool name prefix / explicit map.
//!
//! Sub-agent registries (`role_registry`, `agent_registry`) are **not** filtered — only the
//! top-level surface that pays the per-turn schema cost.

use crate::agent::tools::ToolRegistry;
use crate::core::cli_config::{self, CliConfig};

/// One Hermes-like bundle (see `platform_toolsets.cli` in Hermes `config.yaml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolsetInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub blurb: &'static str,
}

/// Catalog order matches Hermes CLI bundles where aizen has an analogue (+ `mcp`, `lsp`).
pub const CATALOG: &[ToolsetInfo] = &[
    ToolsetInfo {
        id: "memory",
        label: "memory",
        blurb: "recall facts, profile, dialectic Q&A",
    },
    ToolsetInfo {
        id: "file",
        label: "file",
        blurb: "read, glob, search, edit, write, move",
    },
    ToolsetInfo {
        id: "terminal",
        label: "terminal",
        blurb: "shell_run + background process pool",
    },
    ToolsetInfo {
        id: "web",
        label: "web",
        blurb: "search, fetch, crawl (Reach backends)",
    },
    ToolsetInfo {
        id: "skills",
        label: "skills",
        blurb: "load/save/refine + marketplace search/install",
    },
    ToolsetInfo {
        id: "delegation",
        label: "delegation",
        blurb: "task sub-agents + workflow fan-out",
    },
    ToolsetInfo {
        id: "todo",
        label: "todo",
        blurb: "in-session checklist (todo_write)",
    },
    ToolsetInfo {
        id: "clarify",
        label: "clarify",
        blurb: "ask one question and yield the turn",
    },
    ToolsetInfo {
        id: "checkpoint",
        label: "checkpoint",
        blurb: "git time-machine restore points",
    },
    ToolsetInfo {
        id: "messaging",
        label: "messaging",
        blurb: "telegram + outbound notify (when configured)",
    },
    ToolsetInfo {
        id: "persona",
        label: "persona",
        blurb: "mint/switch character (persona_create)",
    },
    ToolsetInfo {
        id: "mcp",
        label: "mcp",
        blurb: "all MCP app tools (mcp_<server>_<tool>)",
    },
    ToolsetInfo {
        id: "lsp",
        label: "lsp",
        blurb: "references, definition, symbols, diagnostics, symbol_replace/insert, repo_map",
    },
    ToolsetInfo {
        id: "browser",
        label: "browser",
        blurb: "CDP browser automation (--features browser)",
    },
];

/// Map a registered tool name → bundle id. `None` = unknown (left visible when filtering).
pub fn classify_tool(name: &str) -> Option<&'static str> {
    if name.starts_with("mcp_") {
        return Some("mcp");
    }
    match name {
        "memory_search" | "memory_profile" | "memory_ask" => Some("memory"),
        "file_read"
        | "file_glob"
        | "search_files"
        | "file_edit"
        | "multi_edit"
        | "file_write"
        | "file_move" => Some("file"),
        "shell_run" | "process" => Some("terminal"),
        "web_search" | "web_fetch" | "web_crawl" => Some("web"),
        "skill_load" | "skill_save" | "skill_refine" | "skill_search" | "skill_install" => Some("skills"),
        "task" | "workflow" => Some("delegation"),
        "todo_write" => Some("todo"),
        "clarify" => Some("clarify"),
        "checkpoint" | "checkpoint_rewind" | "checkpoint_list" | "checkpoint_restore" => Some("checkpoint"),
        "telegram_send" | "telegram_ask" | "notify" => Some("messaging"),
        "persona_create" => Some("persona"),
        "lsp_references"
        | "lsp_definition"
        | "lsp_document_symbols"
        | "lsp_workspace_symbol"
        | "lsp_diagnostics"
        | "symbol_replace"
        | "symbol_insert"
        | "repo_map" => Some("lsp"),
        "browser_navigate"
        | "browser_snapshot"
        | "browser_click"
        | "browser_type"
        | "browser_eval" => Some("browser"),
        _ => None,
    }
}

fn list_match(hay: &str, list: &[String]) -> bool {
    list.iter().any(|t| t.eq_ignore_ascii_case(hay))
}

/// Is bundle `id` allowed under `cfg`? Empty `enabled_toolsets` ⇒ no whitelist; empty
/// `disabled_toolsets` ⇒ nothing extra disabled.
pub fn toolset_allowed(id: &str, cfg: &CliConfig) -> bool {
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() && !list_match(id, en) {
            return false;
        }
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if list_match(id, dis) {
            return false;
        }
    }
    true
}

/// Drop tools whose bundle is disabled. Unknown tools are kept (forward-compatible).
pub fn apply_toolset_filter(registry: &mut ToolRegistry) {
    let cfg = cli_config::load();
    registry.retain(|name| {
        classify_tool(name)
            .map(|ts| toolset_allowed(ts, &cfg))
            .unwrap_or(true)
    });
}

/// Human summary for `/tools` and `config show`.
pub fn format_status(registry: &ToolRegistry) -> String {
    let cfg = cli_config::load();
    let names = registry.names();
    let mut by_ts: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    let mut other: Vec<String> = Vec::new();
    for n in &names {
        match classify_tool(n) {
            Some(ts) => by_ts.entry(ts).or_default().push(n.clone()),
            None => other.push(n.clone()),
        }
    }
    let mut out = String::new();
    out.push_str(&format!("{} tool(s) advertised this session\n", names.len()));
    for info in CATALOG {
        let allowed = toolset_allowed(info.id, &cfg);
        let count = by_ts.get(info.id).map(|v| v.len()).unwrap_or(0);
        let mark = if allowed {
            if count > 0 {
                "●"
            } else {
                "○"
            }
        } else {
            "✗"
        };
        let state = if allowed {
            if count > 0 {
                format!("{count} tool(s)")
            } else {
                "none registered (config off or not available)".to_string()
            }
        } else {
            "disabled in config".to_string()
        };
        out.push_str(&format!("  {mark} {} — {}  [{state}]\n", info.id, info.blurb));
    }
    if !other.is_empty() {
        out.push_str(&format!("  ? unclassified: {}\n", other.join(", ")));
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if !dis.is_empty() {
            out.push_str(&format!("\ndisabled_toolsets: {}\n", dis.join(", ")));
        }
    }
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() {
            out.push_str(&format!("enabled_toolsets (whitelist): {}\n", en.join(", ")));
        }
    }
    out.push_str("\nconfig: `aizen config set --disabled-toolsets web,browser` or /tools in the REPL\n");
    out.push_str("apps: connect more tools via /apps (MCP) — bundle `mcp`\n");
    out.trim_end().to_string()
}

/// Config-only status (no registry build — safe for `/tools status` without MCP connect).
pub fn format_config_status() -> String {
    let cfg = cli_config::load();
    let mut out = String::from("Tool bundles (Hermes-style platform_toolsets):\n");
    for info in CATALOG {
        let allowed = toolset_allowed(info.id, &cfg);
        let mark = if allowed { "●" } else { "✗" };
        out.push_str(&format!("  {mark} {:<12} {}\n", info.id, info.blurb));
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if !dis.is_empty() {
            out.push_str(&format!("\ndisabled: {}\n", dis.join(", ")));
        }
    }
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() {
            out.push_str(&format!("whitelist: {}\n", en.join(", ")));
        }
    }
    out.push_str("\nMore tools: /apps (MCP). Shrink schema: config set --disabled-toolsets web,browser\n");
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_core_bundles() {
        assert_eq!(classify_tool("memory_search"), Some("memory"));
        assert_eq!(classify_tool("file_edit"), Some("file"));
        assert_eq!(classify_tool("shell_run"), Some("terminal"));
        assert_eq!(classify_tool("web_search"), Some("web"));
        assert_eq!(classify_tool("task"), Some("delegation"));
        assert_eq!(classify_tool("mcp_github_issues"), Some("mcp"));
        assert_eq!(classify_tool("lsp_references"), Some("lsp"));
        assert_eq!(classify_tool("symbol_replace"), Some("lsp"));
        assert_eq!(classify_tool("symbol_insert"), Some("lsp"));
        assert_eq!(classify_tool("repo_map"), Some("lsp"));
    }

    #[test]
    fn disabled_hides_bundle() {
        let cfg = CliConfig {
            disabled_toolsets: Some(vec!["web".into()]),
            ..Default::default()
        };
        assert!(!toolset_allowed("web", &cfg));
        assert!(toolset_allowed("file", &cfg));
    }

    #[test]
    fn whitelist_overrides() {
        let cfg = CliConfig {
            enabled_toolsets: Some(vec!["memory".into(), "file".into()]),
            ..Default::default()
        };
        assert!(toolset_allowed("memory", &cfg));
        assert!(!toolset_allowed("web", &cfg));
    }
}