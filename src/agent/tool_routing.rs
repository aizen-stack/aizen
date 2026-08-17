//! The **one** name→capability table, and the compact routing map generated from it.
//!
//! Two things used to be written by hand and drift apart:
//!
//! 1. `# Tool catalog` — a ~7.3 KB prose list of every tool, baked into `system_prompt.md`. It named
//!    tools that a given session does not register (`browser_*` without `--features browser`,
//!    `telegram_*` with no bot configured, `lsp_*` after `/lsp off`, `workflow` when opted out) and
//!    silently omitted ones it does (`memory_list`, `read_symbol`, `lsp_hover`, `goal_complete`,
//!    `bot_admin`). Describing a tool the model cannot call invites a call that can only come back as
//!    `error: unknown tool`.
//! 2. `toolsets::classify_tool` — name → config bundle id, for `disabled_toolsets`.
//!
//! Both are now views over [`lane_for`]. A [`Lane`] is the fine-grained classification: it maps 1:1
//! onto the config bundle id (so `cli-config.json` keeps working byte-for-byte) and N:1 onto a
//! [`Group`], the coarser heading the routing map prints. `symbol_replace` is why the two levels
//! exist — it belongs to the `lsp` bundle for config purposes but reads as *editing* to the model.
//!
//! The map itself is built from the **enabled** registry's advertised names, in registry order, so
//! the names in the prompt are exactly the names in the request's `tools` array. Nothing here knows
//! about schemas: those ride on the request, and duplicating them as prose is what the catalog was.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Fine-grained capability lane. 1:1 with a config bundle id ([`Lane::toolset`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    MemoryRead,
    MemoryWrite,
    FileRead,
    FileWrite,
    Shell,
    Process,
    Web,
    Browser,
    Skills,
    Delegation,
    Coordination,
    Todo,
    Clarify,
    Checkpoint,
    Messaging,
    Persona,
    Mcp,
    Structure,
    LspNav,
    LspEdit,
}

impl Lane {
    /// The `platform_toolsets` bundle id this lane belongs to. These strings are user-visible
    /// (`disabled_toolsets` / `enabled_toolsets` in `cli-config.json`) — changing one silently
    /// invalidates a user's config, so they are pinned by `toolset_ids_are_stable`.
    pub fn toolset(self) -> &'static str {
        match self {
            Lane::MemoryRead | Lane::MemoryWrite => "memory",
            Lane::FileRead | Lane::FileWrite => "file",
            Lane::Shell | Lane::Process => "terminal",
            Lane::Web => "web",
            Lane::Browser => "browser",
            Lane::Skills => "skills",
            Lane::Delegation | Lane::Coordination => "delegation",
            Lane::Todo => "todo",
            Lane::Clarify => "clarify",
            Lane::Checkpoint => "checkpoint",
            Lane::Messaging => "messaging",
            Lane::Persona => "persona",
            Lane::Mcp => "mcp",
            Lane::Structure | Lane::LspNav | Lane::LspEdit => "lsp",
        }
    }

    /// The routing-map heading this lane prints under.
    fn group(self) -> Group {
        match self {
            Lane::MemoryRead | Lane::MemoryWrite => Group::Memory,
            Lane::Structure => Group::Structure,
            Lane::FileRead => Group::Discovery,
            Lane::LspNav => Group::CodeIntel,
            Lane::FileWrite | Lane::LspEdit => Group::Editing,
            Lane::Shell => Group::Exec,
            Lane::Process => Group::LongRunning,
            Lane::Web => Group::Web,
            Lane::Browser => Group::Browser,
            Lane::Todo => Group::Planning,
            Lane::Delegation => Group::Delegation,
            Lane::Coordination => Group::Coordination,
            Lane::Clarify => Group::Clarify,
            Lane::Skills => Group::Skills,
            Lane::Checkpoint => Group::Checkpoint,
            Lane::Messaging => Group::Channels,
            Lane::Persona => Group::Persona,
            Lane::Mcp => Group::Mcp,
        }
    }
}

/// A heading in the routing map. `ORDER` is the print order and is deliberately the order a task
/// tends to move through — recall, locate, understand, change, verify — so the map reads as the
/// operating loop rather than as an alphabetized inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Memory,
    Structure,
    Discovery,
    CodeIntel,
    Editing,
    Exec,
    LongRunning,
    Web,
    Browser,
    Planning,
    Delegation,
    Coordination,
    Clarify,
    Skills,
    Checkpoint,
    Channels,
    Persona,
    Mcp,
    Other,
}

/// Deterministic print order. Prefix-cache stability depends on this never being derived from a
/// hash-map iteration.
const ORDER: &[Group] = &[
    Group::Memory,
    Group::Structure,
    Group::Discovery,
    Group::CodeIntel,
    Group::Editing,
    Group::Exec,
    Group::LongRunning,
    Group::Web,
    Group::Browser,
    Group::Planning,
    Group::Delegation,
    Group::Coordination,
    Group::Clarify,
    Group::Skills,
    Group::Checkpoint,
    Group::Channels,
    Group::Persona,
    Group::Mcp,
    Group::Other,
];

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Memory => "memory & past decisions",
            Group::Structure => "repo structure & indexed search",
            Group::Discovery => "find & read files",
            Group::CodeIntel => "code intelligence",
            Group::Editing => "editing source",
            Group::Exec => "commands, builds, tests, git",
            Group::LongRunning => "long-running processes",
            Group::Web => "web research",
            Group::Browser => "browser (JS / login / interaction)",
            Group::Planning => "planning & progress",
            Group::Delegation => "delegation",
            Group::Coordination => "multi-window coordination",
            Group::Clarify => "asking the user",
            Group::Skills => "skills (reusable procedures)",
            Group::Checkpoint => "checkpoints (time machine)",
            Group::Channels => "reaching the user elsewhere",
            Group::Persona => "persona",
            Group::Mcp => "connected integrations (MCP)",
            Group::Other => "other tools advertised this session",
        }
    }

    /// One line of routing guidance, printed under the group.
    ///
    /// **No tool names here.** The names live on the group line and come from the live registry; a
    /// name inside a hint would be a second, unfiltered source that could advertise a tool this
    /// session did not register. Shell commands (find, dir /s) are named freely — they are not tools.
    fn hint(self) -> &'static str {
        match self {
            Group::Memory => {
                "recall before you rediscover: the user's stored preferences, decisions and gotchas. \
                 Ask memory instead of re-asking the user. Write only durable, reusable facts."
            }
            Group::Structure => {
                "first move in an unfamiliar repo: get the shape before opening files. Code \
                 identifiers are English, so search an English identifier even when the question is \
                 in another language."
            }
            Group::Discovery => {
                "glob by name or pattern to locate, regex-search to pinpoint, then read only the \
                 slice you need. Start narrow and widen only if it misses. Never shell out to find a \
                 file (find, fd, where, dir /s, Get-ChildItem -Recurse): slow on a big tree and not \
                 installed everywhere."
            }
            Group::CodeIntel => {
                "a semantic question about code is not a grep: who calls it, where it is defined, \
                 what a file contains, what broke after an edit. Prefer an outline or one symbol body \
                 over dumping a whole file."
            }
            Group::Editing => {
                "smallest patch that works; match the file's existing style and libraries. Rewrite a \
                 whole named item by symbol; use exact-string replacement for a small region, \
                 batching several edits to one file into a single call. Read before you overwrite. \
                 NEVER create, blank or overwrite a file through the shell — that loses data."
            }
            Group::Exec => {
                "use the shell named in <environment>. It is fully available for builds, tests, git, \
                 moving files, and for opening things — just run it rather than telling the user to. \
                 Quote paths with spaces."
            }
            Group::LongRunning => {
                "a dev server, watcher or long build belongs here rather than blocking the turn."
            }
            Group::Web => {
                "only for what the repo and memory cannot answer. Batch distinct angles into one \
                 search instead of firing sequential ones, read snippets before fetching, extract the \
                 answer rather than dumping the page, and cite the URL. Cross-check a second source \
                 when the fact matters."
            }
            Group::Browser => {
                "only when the content is behind JavaScript, a login, or an interaction a fetch \
                 cannot perform."
            }
            Group::Planning => {
                "plan by blast radius, not step count: multiple files or hard to undo earns a short \
                 visible list (<=5 items, exactly one in progress). A long but single-file, easily \
                 reversible edit does not. Execute the list; don't re-plan it every turn."
            }
            Group::Delegation => {
                "hand a child ONE complete, self-contained job and get back only its result. Fan out \
                 when angles, subsystems or file groups are genuinely independent; keep writers \
                 singular on a shared working tree. A child cannot delegate further."
            }
            Group::Coordination => {
                "other windows may be editing this repository right now; check before a wide change."
            }
            Group::Clarify => {
                "only for genuine ambiguity where a wrong guess wastes real work. Otherwise discover \
                 the answer instead of asking."
            }
            Group::Skills => {
                "look for an existing procedure before building one from scratch; capture a newly \
                 worked-out, reusable one after."
            }
            Group::Checkpoint => {
                "inspect what changed before you undo anything — a rewind discards every change since \
                 the anchor. The runtime already snapshots before the first destructive op and at \
                 phase boundaries, so add one only for risk it cannot see. None of this restores chat \
                 history."
            }
            Group::Channels => {
                "use sparingly — a needless ping is worse than silence."
            }
            Group::Persona => "only when the user asks for a durable role or voice.",
            Group::Mcp => {
                "tools from servers connected to this session (databases, APIs, project tooling). \
                 Prefer a matching one over a generic shell hack."
            }
            Group::Other => {
                "not classified into a lane above; read its description on the request before using \
                 it."
            }
        }
    }
}

/// Classify a registered tool name. `None` ⇒ unknown to this table; it is still advertised to the
/// model (forward-compatible) and prints under `Group::Other`, but no toolset filter applies to it.
///
/// This is the ONLY name→capability table in the codebase. `toolsets::classify_tool` and the prompt's
/// routing map are both views over it, which is what stops the config bundles and the prompt from
/// disagreeing about what a tool is.
pub fn lane_for(name: &str) -> Option<Lane> {
    if name.starts_with("mcp_") {
        return Some(Lane::Mcp);
    }
    Some(match name {
        "memory_search" | "memory_list" | "memory_profile" | "memory_ask" => Lane::MemoryRead,
        "memory_save" | "memory_update" | "memory_forget" => Lane::MemoryWrite,
        "file_read" | "file_glob" | "search_files" => Lane::FileRead,
        "file_edit" | "file_write" | "file_move" => Lane::FileWrite,
        "shell_run" => Lane::Shell,
        "process" => Lane::Process,
        "web_search" | "web_fetch" | "web_crawl" => Lane::Web,
        "browser_navigate" | "browser_snapshot" | "browser_click" | "browser_type"
        | "browser_eval" => Lane::Browser,
        "skill_load" | "skill_save" | "skill_refine" | "skill_forget" | "skill_search"
        | "skill_install" => Lane::Skills,
        "task" | "workflow" => Lane::Delegation,
        "team_status" => Lane::Coordination,
        "todo_write" | "goal_complete" => Lane::Todo,
        "clarify" => Lane::Clarify,
        "checkpoint" | "checkpoint_view" => Lane::Checkpoint,
        "telegram_send" | "telegram_ask" | "bot_admin" | "notify" => Lane::Messaging,
        "persona_create" => Lane::Persona,
        "repo_map" | "codebase_search" => Lane::Structure,
        "lsp_references"
        | "lsp_definition"
        | "read_symbol"
        | "lsp_hover"
        | "lsp_document_symbols"
        | "lsp_workspace_symbol"
        | "lsp_diagnostics" => Lane::LspNav,
        "symbol_replace" | "symbol_insert" => Lane::LspEdit,
        _ => return None,
    })
}

/// The full opening line of a GENERATED routing map. The parenthetical is load-bearing: the static
/// base prompt mentions `# Tool routing` in prose (it tells the model where to look), so a bare
/// heading match would report a map present in a prompt that has none.
pub const ROUTING_HEADING: &str = "# Tool routing (this session's live surface)";

/// Build the compact routing map for exactly these advertised tool names.
///
/// `names` must be the registry's advertised order — the same list whose schemas go on the request —
/// so a name in the prompt is always a name the model may actually call. Returns `None` for an empty
/// surface (a registry with no tools gets no heading rather than an empty one).
///
/// Deterministic: groups print in [`ORDER`], names inside a group keep registry order. Repeated calls
/// with the same surface return the cached string, so a per-turn prompt rebuild costs a hash rather
/// than a rebuild.
pub fn routing_map(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let key = surface_key(names);
    static CACHE: Mutex<Option<(u64, String)>> = Mutex::new(None);
    {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, cached)) = guard.as_ref() {
            if *k == key {
                return Some(cached.clone());
            }
        }
    }
    let built = render(names);
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, built.clone()));
    Some(built)
}

/// Order-sensitive fingerprint of a tool surface (the memo key).
fn surface_key(names: &[String]) -> u64 {
    let mut h = DefaultHasher::new();
    names.len().hash(&mut h);
    for n in names {
        n.hash(&mut h);
    }
    h.finish()
}

fn render(names: &[String]) -> String {
    let mut buckets: Vec<(Group, Vec<&str>)> = ORDER.iter().map(|g| (*g, Vec::new())).collect();
    for n in names {
        let g = lane_for(n).map(Lane::group).unwrap_or(Group::Other);
        if let Some(slot) = buckets.iter_mut().find(|(bg, _)| *bg == g) {
            slot.1.push(n.as_str());
        }
    }
    let mut out = String::with_capacity(2048);
    out.push_str(ROUTING_HEADING);
    out.push_str(
        "\nThese are the only tools you can call, and their exact names. Full argument schemas ride \
         on the request — read those there; never guess an argument or invent a tool. A capability \
         missing below is not available this session: say so instead of pretending, and never claim \
         a tool was blocked unless a result said so.\n",
    );
    for (g, tools) in buckets.iter().filter(|(_, t)| !t.is_empty()) {
        out.push_str("- ");
        out.push_str(g.label());
        out.push_str(" → ");
        for (i, t) in tools.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('`');
            out.push_str(t);
            out.push('`');
        }
        out.push_str("\n  ");
        out.push_str(g.hint());
        out.push('\n');
    }
    out.push_str(
        "Pick the sharpest tool for the operation and never shell out for something a dedicated tool \
         does. Batch independent reads into one turn; sequence only when one result feeds the next.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Every backticked token in a rendered map (the map's only use of backticks is a tool name).
    fn backticked(map: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = map;
        while let Some(i) = rest.find('`') {
            rest = &rest[i + 1..];
            match rest.find('`') {
                Some(j) => {
                    out.push(rest[..j].to_string());
                    rest = &rest[j + 1..];
                }
                None => break,
            }
        }
        out
    }

    #[test]
    fn map_lists_exactly_the_enabled_names() {
        let surface = names(&["memory_search", "file_read", "file_edit", "shell_run"]);
        let map = routing_map(&surface).expect("non-empty surface renders");
        assert_eq!(
            backticked(&map),
            surface,
            "every backticked token is an enabled tool, in registry order"
        );
        assert!(!map.contains("web_search"), "absent tool must not appear");
    }

    #[test]
    fn hints_never_name_a_tool() {
        // A hint is static prose; a tool name inside one would be a second, unfiltered advertisement.
        // Rendering the WHOLE table and checking that no hint line carries a backtick proves it.
        let all: Vec<String> = names(&[
            "memory_search",
            "memory_save",
            "repo_map",
            "file_read",
            "lsp_hover",
            "symbol_replace",
            "file_edit",
            "shell_run",
            "process",
            "web_search",
            "browser_eval",
            "todo_write",
            "task",
            "team_status",
            "clarify",
            "skill_load",
            "checkpoint",
            "notify",
            "persona_create",
            "mcp_db_query",
            "some_future_tool",
        ]);
        let map = routing_map(&all).unwrap();
        for line in map.lines().filter(|l| l.starts_with("  ")) {
            assert!(
                !line.contains('`'),
                "hint line must not name a tool: {line}"
            );
        }
        // …and every group heading did render, so the check above covered all of them.
        for g in ORDER {
            assert!(map.contains(g.label()), "group {:?} missing", g);
        }
    }

    #[test]
    fn ordering_is_deterministic_and_independent_of_input_grouping() {
        let a = routing_map(&names(&["shell_run", "memory_search", "file_read"])).unwrap();
        let b = routing_map(&names(&["shell_run", "memory_search", "file_read"])).unwrap();
        assert_eq!(a, b, "same surface ⇒ byte-identical map (prefix cache)");
        // Groups print in ORDER regardless of the order the tools were registered in.
        let mem = a.find("memory_search").unwrap();
        let file = a.find("file_read").unwrap();
        let sh = a.find("shell_run").unwrap();
        assert!(
            mem < file && file < sh,
            "groups follow ORDER, not input order"
        );
    }

    #[test]
    fn unknown_tools_land_in_other_not_dropped() {
        let map = routing_map(&names(&["totally_new_tool"])).unwrap();
        assert!(map.contains("`totally_new_tool`"));
        assert!(map.contains(Group::Other.label()));
    }

    #[test]
    fn mcp_tools_group_under_integrations() {
        let map = routing_map(&names(&["mcp_github_create_issue"])).unwrap();
        assert!(map.contains(Group::Mcp.label()));
        assert!(map.contains("`mcp_github_create_issue`"));
    }

    #[test]
    fn empty_surface_renders_nothing() {
        assert!(routing_map(&[]).is_none());
    }

    /// The whole point of replacing the catalog was to stop paying for prose that duplicates the
    /// schemas already on the request. This renders a MAXIMAL surface — every group populated, which
    /// no real session has (browser needs a feature flag, telegram a bot, MCP a server) — and still
    /// comes in under the ~7.3 KB the hand-written catalog cost unconditionally. A real top-level
    /// surface of ~40 tools measures ≈4.5 KB. Printed so `--nocapture` shows what the model sees.
    #[test]
    fn a_full_surface_stays_compact() {
        let all: Vec<String> = [
            "memory_search",
            "memory_list",
            "memory_profile",
            "memory_ask",
            "memory_save",
            "memory_update",
            "memory_forget",
            "repo_map",
            "codebase_search",
            "file_glob",
            "search_files",
            "file_read",
            "lsp_workspace_symbol",
            "lsp_document_symbols",
            "lsp_definition",
            "lsp_references",
            "read_symbol",
            "lsp_hover",
            "lsp_diagnostics",
            "file_edit",
            "file_write",
            "file_move",
            "symbol_replace",
            "symbol_insert",
            "shell_run",
            "process",
            "web_search",
            "web_fetch",
            "web_crawl",
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_eval",
            "todo_write",
            "goal_complete",
            "task",
            "workflow",
            "team_status",
            "clarify",
            "skill_search",
            "skill_load",
            "skill_save",
            "skill_refine",
            "skill_forget",
            "skill_install",
            "checkpoint",
            "checkpoint_view",
            "notify",
            "telegram_send",
            "telegram_ask",
            "bot_admin",
            "persona_create",
            "mcp_github_create_issue",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let map = routing_map(&all).unwrap();
        println!("{map}");
        assert!(
            map.len() < 5_200,
            "routing map grew to {} B — it must stay under the 7.3 KB catalog it replaced",
            map.len()
        );
    }

    #[test]
    fn every_lane_maps_to_a_live_toolset_id() {
        // The bundle ids are user config. A lane pointing at an id the catalog doesn't list would
        // make its tools unfilterable (and `/tools` would show a bundle that never matches).
        const LANES: &[Lane] = &[
            Lane::MemoryRead,
            Lane::MemoryWrite,
            Lane::FileRead,
            Lane::FileWrite,
            Lane::Shell,
            Lane::Process,
            Lane::Web,
            Lane::Browser,
            Lane::Skills,
            Lane::Delegation,
            Lane::Coordination,
            Lane::Todo,
            Lane::Clarify,
            Lane::Checkpoint,
            Lane::Messaging,
            Lane::Persona,
            Lane::Mcp,
            Lane::Structure,
            Lane::LspNav,
            Lane::LspEdit,
        ];
        for l in LANES {
            let id = l.toolset();
            assert!(
                crate::agent::toolsets::CATALOG.iter().any(|c| c.id == id),
                "lane {l:?} → unknown bundle {id}"
            );
        }
    }
}
